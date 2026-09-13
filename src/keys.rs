//! Keyboard definitions for `Input.dispatchKeyEvent`: Playwright-style key names ("Enter",
//! "ArrowLeft", "a", "Control+Shift+K", "ControlOrMeta+A") resolved to key/code/keyCode/text.

pub const ALT: i64 = 1;
pub const CONTROL: i64 = 2;
pub const META: i64 = 4;
pub const SHIFT: i64 = 8;

#[derive(Debug, Clone)]
pub struct KeyDef {
    pub key: String,
    pub code: String,
    pub key_code: i64,
    /// Text inserted by the key, if any (only sent when no Control/Meta modifier is held).
    pub text: Option<String>,
    pub location: i64,
}

pub struct Combo {
    pub modifiers: Vec<KeyDef>,
    pub mask: i64,
    pub key: KeyDef,
}

fn named(name: &str) -> Option<KeyDef> {
    let (key, code, key_code, text, location): (&str, &str, i64, Option<&str>, i64) = match name {
        "Enter" => ("Enter", "Enter", 13, Some("\r"), 0),
        "Tab" => ("Tab", "Tab", 9, None, 0),
        "Backspace" => ("Backspace", "Backspace", 8, None, 0),
        "Delete" => ("Delete", "Delete", 46, None, 0),
        "Escape" => ("Escape", "Escape", 27, None, 0),
        "Space" | " " => (" ", "Space", 32, Some(" "), 0),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, None, 0),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, None, 0),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, None, 0),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, None, 0),
        "Home" => ("Home", "Home", 36, None, 0),
        "End" => ("End", "End", 35, None, 0),
        "PageUp" => ("PageUp", "PageUp", 33, None, 0),
        "PageDown" => ("PageDown", "PageDown", 34, None, 0),
        "Insert" => ("Insert", "Insert", 45, None, 0),
        "CapsLock" => ("CapsLock", "CapsLock", 20, None, 0),
        "Shift" => ("Shift", "ShiftLeft", 16, None, 1),
        "Control" => ("Control", "ControlLeft", 17, None, 1),
        "Alt" => ("Alt", "AltLeft", 18, None, 1),
        "Meta" => ("Meta", "MetaLeft", 91, None, 1),
        _ => {
            let n: i64 = name.strip_prefix('F')?.parse().ok().filter(|n| (1..=12).contains(n))?;
            return Some(KeyDef { key: name.into(), code: name.into(), key_code: 111 + n, text: None, location: 0 });
        }
    };
    Some(KeyDef { key: key.into(), code: code.into(), key_code, text: text.map(Into::into), location })
}

/// A single character as its US-layout key.
pub fn char_key(c: char) -> KeyDef {
    let (code, key_code) = match c {
        'a'..='z' | 'A'..='Z' => (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as i64),
        '0'..='9' => (format!("Digit{c}"), c as i64),
        ' ' => ("Space".into(), 32),
        '\n' | '\r' => return named("Enter").unwrap(),
        '\t' => return named("Tab").unwrap(),
        '-' => ("Minus".into(), 189),
        '=' => ("Equal".into(), 187),
        ',' => ("Comma".into(), 188),
        '.' => ("Period".into(), 190),
        '/' => ("Slash".into(), 191),
        ';' => ("Semicolon".into(), 186),
        '\'' => ("Quote".into(), 222),
        '[' => ("BracketLeft".into(), 219),
        ']' => ("BracketRight".into(), 221),
        '\\' => ("Backslash".into(), 220),
        '`' => ("Backquote".into(), 192),
        _ => (String::new(), 0),
    };
    KeyDef { key: c.to_string(), code, key_code, text: Some(c.to_string()), location: 0 }
}

pub fn parse(combo: &str) -> Result<Combo, String> {
    // "Control++" means Control and the plus key, so split on '+' only between tokens.
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in combo.chars() {
        if c == '+' && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    let Some(last) = parts.pop() else { return Err("empty key".into()) };

    let mut modifiers = Vec::new();
    let mut mask = 0;
    for m in parts {
        let m = match m.as_str() {
            "ControlOrMeta" => if cfg!(target_os = "macos") { "Meta" } else { "Control" },
            "Ctrl" => "Control",
            "Cmd" | "Command" => "Meta",
            "Option" => "Alt",
            other => other,
        };
        mask |= match m {
            "Alt" => ALT,
            "Control" => CONTROL,
            "Meta" => META,
            "Shift" => SHIFT,
            _ => return Err(format!("unknown modifier \"{m}\" in \"{combo}\"")),
        };
        modifiers.push(named(m).unwrap());
    }

    let mut key = match named(&last) {
        Some(k) => k,
        None if last.chars().count() == 1 => char_key(last.chars().next().unwrap()),
        None => return Err(format!("unknown key \"{last}\"")),
    };
    if mask & SHIFT != 0 && key.key.len() == 1 {
        key.key = key.key.to_uppercase();
        key.text = key.text.map(|t| t.to_uppercase());
    }
    if mask & (CONTROL | META) != 0 {
        key.text = None;
    }
    Ok(Combo { modifiers, mask, key })
}

/// Editing commands a real macOS keypress would trigger through the responder chain; synthetic
/// key events need them spelled out or Cmd+A/C/V/X/Z do nothing in text fields.
pub fn mac_commands(mask: i64, key: &str) -> Vec<&'static str> {
    if !cfg!(target_os = "macos") || mask & META == 0 {
        return Vec::new();
    }
    match (key.to_ascii_lowercase().as_str(), mask & SHIFT != 0) {
        ("a", _) => vec!["selectAll"],
        ("c", _) => vec!["copy"],
        ("x", _) => vec!["cut"],
        ("v", _) => vec!["paste"],
        ("z", false) => vec!["undo"],
        ("z", true) => vec!["redo"],
        ("backspace", _) => vec!["deleteToBeginningOfLine"],
        ("arrowleft", false) => vec!["moveToBeginningOfLine"],
        ("arrowright", false) => vec!["moveToEndOfLine"],
        _ => Vec::new(),
    }
}
