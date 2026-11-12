//! On-screen keyboards and their D-pad navigation.

use slint::{Image, ModelRc, VecModel};

use crate::{Icons, KeyCap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Space,
    Backspace,
    Clear,
    Done,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct KeyDef {
    pub label: &'static str,
    pub action: KeyAction,
    pub col: i32,
    pub row: i32,
    pub span: i32,
}

#[derive(Debug, Clone)]
pub struct Keyboard {
    pub keys: Vec<KeyDef>,
    pub index: usize,
    /// Column the user is "aiming" at, kept while moving vertically.
    aim_col: i32,
}

const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz1234567890";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn char_label(c: char) -> &'static str {
    // Leak-free static labels for single characters.
    const ALL: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let i = ALL.find(c).unwrap_or(0);
    &ALL[i..i + 1]
}

impl Keyboard {
    /// Search: 6 columns, a–z and digits, then Space / Delete / Clear.
    pub fn search() -> Self {
        let mut keys: Vec<KeyDef> = LETTERS
            .chars()
            .enumerate()
            .map(|(i, c)| KeyDef {
                label: char_label(c),
                action: KeyAction::Char(c),
                col: (i % 6) as i32,
                row: (i / 6) as i32,
                span: 1,
            })
            .collect();
        keys.push(KeyDef { label: "space", action: KeyAction::Space, col: 0, row: 6, span: 2 });
        keys.push(KeyDef { label: "delete", action: KeyAction::Backspace, col: 2, row: 6, span: 2 });
        keys.push(KeyDef { label: "Clear", action: KeyAction::Clear, col: 4, row: 6, span: 2 });
        Self { keys, index: 0, aim_col: 0 }
    }

    /// Real-Debrid key entry: 10 columns, A–Z and digits (keys are uppercase alphanumeric).
    pub fn api_key() -> Self {
        let mut keys: Vec<KeyDef> = UPPER
            .chars()
            .enumerate()
            .map(|(i, c)| KeyDef {
                label: char_label(c),
                action: KeyAction::Char(c),
                col: (i % 10) as i32,
                row: (i / 10) as i32,
                span: 1,
            })
            .collect();
        keys.push(KeyDef { label: "delete", action: KeyAction::Backspace, col: 6, row: 3, span: 2 });
        keys.push(KeyDef { label: "Clear", action: KeyAction::Clear, col: 8, row: 3, span: 2 });
        keys.push(KeyDef { label: "Done", action: KeyAction::Done, col: 0, row: 4, span: 5 });
        keys.push(KeyDef { label: "Cancel", action: KeyAction::Cancel, col: 5, row: 4, span: 5 });
        Self { keys, index: 0, aim_col: 0 }
    }

    pub fn current(&self) -> &KeyDef {
        &self.keys[self.index]
    }

    fn rows(&self) -> i32 {
        self.keys.iter().map(|k| k.row).max().unwrap_or(0) + 1
    }

    fn cols(&self) -> i32 {
        self.keys.iter().map(|k| k.col + k.span).max().unwrap_or(1)
    }

    fn key_at(&self, row: i32, col: i32) -> Option<usize> {
        self.keys
            .iter()
            .position(|k| k.row == row && col >= k.col && col < k.col + k.span)
    }

    /// Move focus. Returns false when the move leaves the keyboard on that side.
    pub fn step(&mut self, dx: i32, dy: i32) -> bool {
        let key = self.current().clone();
        if dx != 0 {
            let col = if dx > 0 { key.col + key.span } else { key.col - 1 };
            if col < 0 || col >= self.cols() {
                return false;
            }
            match self.key_at(key.row, col) {
                Some(i) => {
                    self.index = i;
                    self.aim_col = self.keys[i].col;
                    true
                }
                None => false,
            }
        } else {
            let row = key.row + dy;
            if row < 0 || row >= self.rows() {
                return false;
            }
            let aim = self.aim_col.clamp(key.col, key.col + key.span - 1);
            let target = self
                .key_at(row, aim)
                .or_else(|| (0..self.cols()).rev().find_map(|c| (c <= aim).then(|| self.key_at(row, c)).flatten()));
            match target {
                Some(i) => {
                    self.index = i;
                    true
                }
                None => false,
            }
        }
    }

    pub fn model(&self, icons: &Icons) -> ModelRc<KeyCap> {
        let caps: Vec<KeyCap> = self
            .keys
            .iter()
            .map(|k| {
                let icon: Option<Image> = match k.action {
                    KeyAction::Space => Some(icons.get_space()),
                    KeyAction::Backspace => Some(icons.get_backspace()),
                    _ => None,
                };
                KeyCap {
                    label: k.label.into(),
                    has_icon: icon.is_some(),
                    icon: icon.unwrap_or_default(),
                    col: k.col,
                    row: k.row,
                    span: k.span,
                }
            })
            .collect();
        ModelRc::new(VecModel::from(caps))
    }
}

/// Applies a key action to a text buffer. Returns true when the text changed.
pub fn apply(text: &mut String, action: KeyAction, max_len: usize) -> bool {
    match action {
        KeyAction::Char(c) if text.chars().count() < max_len => {
            text.push(c);
            true
        }
        KeyAction::Space if !text.is_empty() && !text.ends_with(' ') && text.len() < max_len => {
            text.push(' ');
            true
        }
        KeyAction::Backspace => text.pop().is_some(),
        KeyAction::Clear if !text.is_empty() => {
            text.clear();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_keyboard_navigation() {
        let mut kb = Keyboard::search();
        assert_eq!(kb.current().label, "a");
        assert!(!kb.step(-1, 0), "left edge leaves the keyboard");
        assert!(kb.step(1, 0));
        assert_eq!(kb.current().label, "b");
        for _ in 0..5 {
            kb.step(0, 1);
        }
        assert_eq!(kb.current().label, "6"); // row 5, col 1
        assert!(kb.step(0, 1));
        assert_eq!(kb.current().action, KeyAction::Space); // span 0..2
        assert!(kb.step(1, 0));
        assert_eq!(kb.current().action, KeyAction::Backspace);
        assert!(kb.step(0, -1));
        assert_eq!(kb.current().label, "7"); // aim column 2 → "7"
        assert!(!kb.step(0, -10));
    }

    #[test]
    fn api_keyboard_layout() {
        let mut kb = Keyboard::api_key();
        assert_eq!(kb.keys.len(), 40);
        for _ in 0..4 {
            kb.step(0, 1);
        }
        assert_eq!(kb.current().action, KeyAction::Done);
        assert!(kb.step(1, 0));
        assert_eq!(kb.current().action, KeyAction::Cancel);
    }

    #[test]
    fn applies_actions() {
        let mut text = String::new();
        assert!(apply(&mut text, KeyAction::Char('a'), 5));
        assert!(!apply(&mut text, KeyAction::Space, 1));
        assert!(apply(&mut text, KeyAction::Space, 5));
        assert!(!apply(&mut text, KeyAction::Space, 5));
        assert!(apply(&mut text, KeyAction::Backspace, 5));
        assert!(apply(&mut text, KeyAction::Clear, 5));
        assert!(text.is_empty());
    }
}
