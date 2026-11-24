//! Keys by name (ADR-123 D4): `cmd+a`, `ctrl+shift+tab`, `enter`, `f5`.
//!
//! A name is parsed into a [`Chord`] the same way on every system; each
//! platform maps the chord to its own key codes and never borrows another
//! system's codes. `cmd` is Command on macOS and Ctrl on Windows; `win` is
//! the Windows key, and has no Mac counterpart.

use anyhow::{anyhow, bail, Result};

/// A key other than a modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A letter, digit or punctuation key, by the character it types
    /// without Shift (letters lowercase).
    Char(char),
    Enter,
    Tab,
    Escape,
    Backspace,
    /// Forward delete.
    Delete,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    /// F1 to F20.
    F(u8),
}

impl Key {
    /// The key's name as the tool takes it.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Char(c) => c.to_string(),
            Self::Enter => "enter".into(),
            Self::Tab => "tab".into(),
            Self::Escape => "escape".into(),
            Self::Backspace => "backspace".into(),
            Self::Delete => "delete".into(),
            Self::Space => "space".into(),
            Self::Up => "up".into(),
            Self::Down => "down".into(),
            Self::Left => "left".into(),
            Self::Right => "right".into(),
            Self::Home => "home".into(),
            Self::End => "end".into(),
            Self::PageUp => "pageup".into(),
            Self::PageDown => "pagedown".into(),
            Self::Insert => "insert".into(),
            Self::F(n) => format!("f{n}"),
        }
    }
}

/// Modifiers held for a chord.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    /// Command on macOS, Ctrl on Windows.
    pub cmd: bool,
    pub ctrl: bool,
    /// Option on macOS.
    pub alt: bool,
    pub shift: bool,
    /// The Windows key (Windows only).
    pub win: bool,
}

impl Mods {
    #[must_use]
    pub fn any(self) -> bool {
        self.cmd || self.ctrl || self.alt || self.shift || self.win
    }
}

/// One key with the modifiers held for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub mods: Mods,
    pub key: Key,
}

/// Names of the keys that are not single characters, for the tool's
/// description.
pub const KEY_NAMES: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "backspace",
    "delete",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
    "f1…f20",
];

/// Parse `cmd+shift+a`. `mac` says whether the chord is for macOS (where
/// `win` does not exist).
pub fn parse(spec: &str, mac: bool) -> Result<Chord> {
    let spec = spec.trim();
    anyhow::ensure!(!spec.is_empty(), "name a key, e.g. `enter` or `cmd+a`");
    // `+` alone, or a chord ending in `+` (`cmd++`), names the plus key.
    let (head, last) = if spec == "+" {
        ("", "+")
    } else if let Some(head) = spec.strip_suffix("++") {
        (head, "+")
    } else {
        match spec.rsplit_once('+') {
            Some((head, last)) => (head, last),
            None => ("", spec),
        }
    };
    let mut mods = Mods::default();
    for part in head.split('+').filter(|p| !p.is_empty()) {
        match part.trim().to_ascii_lowercase().as_str() {
            "cmd" | "command" | "meta" => mods.cmd = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "option" | "opt" => mods.alt = true,
            "shift" => mods.shift = true,
            "win" | "windows" | "super" => {
                if mac {
                    bail!("a Mac has no Windows key; use `cmd`");
                }
                mods.win = true;
            }
            other => bail!("`{other}` is not a modifier (cmd, ctrl, alt, shift, win)"),
        }
    }
    let key = key(last)?;
    Ok(Chord { mods, key })
}

fn key(name: &str) -> Result<Key> {
    let lower = name.trim().to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" | "forwarddelete" => Key::Delete,
        "space" => Key::Space,
        "up" | "arrowup" => Key::Up,
        "down" | "arrowdown" => Key::Down,
        "left" | "arrowleft" => Key::Left,
        "right" | "arrowright" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "insert" | "ins" => Key::Insert,
        "plus" => Key::Char('='),
        "minus" => Key::Char('-'),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = f[1..].parse().map_err(|_| anyhow!("no key `{name}`"))?;
            anyhow::ensure!((1..=20).contains(&n), "function keys go from f1 to f20");
            Key::F(n)
        }
        _ => {
            let mut chars = lower.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_graphic() => Key::Char(unshifted(c)),
                _ => bail!(
                    "no key `{name}`: use a single character or one of {}",
                    KEY_NAMES.join(", ")
                ),
            }
        }
    };
    Ok(key)
}

/// The character on the same US-layout key without Shift (`!` → `1`), so
/// a chord names a key, not a character.
fn unshifted(c: char) -> char {
    match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        other => other.to_ascii_lowercase(),
    }
}

/// Whether `c` sits on a shifted key in the US layout (so a chord with it
/// adds Shift).
#[must_use]
pub fn needs_shift(c: char) -> bool {
    c.is_ascii_uppercase() || "!@#$%^&*()_+{}|:\"<>?~".contains(c)
}

/// Parse the chord and add Shift for a shifted character (`cmd+?`).
pub fn parse_with_shift(spec: &str, mac: bool) -> Result<Chord> {
    let mut chord = parse(spec, mac)?;
    let last = spec.trim().chars().last().unwrap_or_default();
    let names_a_character =
        spec.trim().chars().count() == 1 || spec.trim().ends_with(&format!("+{last}"));
    if names_a_character && needs_shift(last) {
        chord.mods.shift = true;
    }
    Ok(chord)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_the_same_everywhere() {
        let c = parse("cmd+a", true).unwrap();
        assert!(c.mods.cmd && !c.mods.shift);
        assert_eq!(c.key, Key::Char('a'));
        let c = parse("Ctrl+Shift+Tab", false).unwrap();
        assert!(c.mods.ctrl && c.mods.shift);
        assert_eq!(c.key, Key::Tab);
        assert_eq!(parse("enter", true).unwrap().key, Key::Enter);
        assert_eq!(parse("f5", false).unwrap().key, Key::F(5));
        assert_eq!(parse("cmd++", true).unwrap().key, Key::Char('='));
        assert!(parse("f21", true).is_err());
        assert!(parse("hyper+a", true).is_err());
        assert!(parse("", true).is_err());
    }

    #[test]
    fn the_windows_key_is_not_on_a_mac() {
        assert!(parse("win+r", true).is_err());
        assert!(parse("win+r", false).unwrap().mods.win);
    }

    #[test]
    fn shifted_characters_add_shift() {
        let c = parse_with_shift("cmd+?", true).unwrap();
        assert!(c.mods.shift && c.mods.cmd);
        assert_eq!(c.key, Key::Char('/'));
        let c = parse_with_shift("A", true).unwrap();
        assert!(c.mods.shift);
        assert_eq!(c.key, Key::Char('a'));
        assert!(!parse_with_shift("a", true).unwrap().mods.shift);
    }
}
