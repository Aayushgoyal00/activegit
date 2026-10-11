//! Turns raw terminal key events into what the app acts on.
//!
//! Bindings are plain [`KeyCode`]s, so a key pressed with Ctrl or Alt must
//! never fall through as the bare key (`Ctrl+D` is not `d`, which discards).
//! Modified keys become a [`Chord`] the app interprets per mode; anything it
//! does not know is dropped.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A key pressed with Ctrl or Alt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    Ctrl(char),
    Alt(char),
    /// Ctrl/Alt + Left.
    WordLeft,
    /// Ctrl/Alt + Right.
    WordRight,
    /// Ctrl/Alt + Backspace.
    DeleteWord,
    /// Alt/Ctrl + Enter: a line break where Enter would submit.
    Newline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A normal key, with whether Shift was held.
    Key(KeyCode, bool),
    Chord(Chord),
    /// Ctrl+C: always quits, whatever the bindings say.
    Quit,
    /// Releases, repeats' release halves, and combos nothing uses.
    Ignored,
}

/// Classify one key event. Shift+letter is folded to the uppercase letter
/// (some terminals report lowercase + SHIFT), so `Q` and `q` stay distinct.
pub fn classify(key: &KeyEvent) -> Input {
    if key.kind != KeyEventKind::Press {
        return Input::Ignored;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // AltGr arrives as Ctrl+Alt on Windows and produces ordinary characters
    // (`@`, `{`, `\` on many layouts): treat it as typing.
    if ctrl && alt {
        return match key.code {
            KeyCode::Char(_) => Input::Key(key.code, shift),
            _ => Input::Ignored,
        };
    }
    if ctrl || alt {
        return match key.code {
            KeyCode::Char('c' | 'C') if ctrl => Input::Quit,
            KeyCode::Char(c) if ctrl => Input::Chord(Chord::Ctrl(c.to_ascii_lowercase())),
            KeyCode::Char(c) => Input::Chord(Chord::Alt(c.to_ascii_lowercase())),
            KeyCode::Left => Input::Chord(Chord::WordLeft),
            KeyCode::Right => Input::Chord(Chord::WordRight),
            KeyCode::Backspace => Input::Chord(Chord::DeleteWord),
            KeyCode::Enter => Input::Chord(Chord::Newline),
            _ => Input::Ignored,
        };
    }
    let code = match key.code {
        KeyCode::Char(c) if shift && c.is_ascii_lowercase() => {
            KeyCode::Char(c.to_ascii_uppercase())
        }
        code => code,
    };
    Input::Key(code, shift)
}

/// Fewest keys in one drained batch that count as a paste.
const PASTE_BURST: usize = 10;

/// Whether a batch of events read in one go is pasted text rather than
/// typing. Windows consoles deliver a paste as ordinary key presses, so a
/// pasted `fix: x\n\ndone` would otherwise submit the commit box at the
/// first newline and run the rest as commands. Nobody types ten keys
/// between two frames; a held key repeats one character, so a burst of a
/// single repeated key is not a paste.
pub fn paste_text(batch: &[Input]) -> Option<String> {
    let mut text = String::new();
    for input in batch {
        match input {
            Input::Key(KeyCode::Char(c), _) => text.push(*c),
            Input::Key(KeyCode::Enter, _) => text.push('\n'),
            Input::Key(KeyCode::Tab, _) => text.push('\t'),
            _ => {}
        }
    }
    let mut distinct: Vec<char> = text.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    (text.chars().count() >= PASTE_BURST && distinct.len() >= 3).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_and_alt_keys_never_become_the_bare_key() {
        // Ctrl+D must not reach the `d` (discard) binding.
        assert_eq!(
            classify(&press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Input::Chord(Chord::Ctrl('d'))
        );
        assert_eq!(
            classify(&press(KeyCode::Char('X'), KeyModifiers::CONTROL)),
            Input::Chord(Chord::Ctrl('x'))
        );
        assert_eq!(
            classify(&press(KeyCode::Char('q'), KeyModifiers::ALT)),
            Input::Chord(Chord::Alt('q'))
        );
        assert_eq!(
            classify(&press(KeyCode::Delete, KeyModifiers::CONTROL)),
            Input::Ignored
        );
    }

    #[test]
    fn ctrl_c_quits_and_altgr_types() {
        assert_eq!(
            classify(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Input::Quit
        );
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(
            classify(&press(KeyCode::Char('@'), altgr)),
            Input::Key(KeyCode::Char('@'), false)
        );
    }

    #[test]
    fn shift_letter_folds_to_uppercase_and_keeps_shift() {
        assert_eq!(
            classify(&press(KeyCode::Char('q'), KeyModifiers::SHIFT)),
            Input::Key(KeyCode::Char('Q'), true)
        );
        assert_eq!(
            classify(&press(KeyCode::Char('A'), KeyModifiers::NONE)),
            Input::Key(KeyCode::Char('A'), false)
        );
        assert_eq!(
            classify(&press(KeyCode::Right, KeyModifiers::SHIFT)),
            Input::Key(KeyCode::Right, true)
        );
    }

    #[test]
    fn releases_are_ignored() {
        let mut key = press(KeyCode::Char('j'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        assert_eq!(classify(&key), Input::Ignored);
    }

    fn keys(text: &str) -> Vec<Input> {
        text.chars()
            .map(|c| match c {
                '\n' => Input::Key(KeyCode::Enter, false),
                c => Input::Key(KeyCode::Char(c), false),
            })
            .collect()
    }

    #[test]
    fn a_burst_of_mixed_keys_is_a_paste() {
        assert_eq!(
            paste_text(&keys("fix: thing\n\nbody")).as_deref(),
            Some("fix: thing\n\nbody")
        );
    }

    #[test]
    fn typing_and_key_repeat_are_not_a_paste() {
        assert_eq!(paste_text(&keys("ab")), None);
        // A held `j` after a stall: many keys, one character.
        assert_eq!(paste_text(&keys("jjjjjjjjjjjjjjjj")), None);
        assert_eq!(paste_text(&keys("jkjkjkjkjkjkjk")), None);
    }
}
