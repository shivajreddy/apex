//! User configuration: `%APPDATA%\apex\config.toml`.
//!
//! Hand-rolled parser for a small TOML subset - `[section]`, `key = value`,
//! `#` comments, quoted strings, booleans. No dependencies, no allocation
//! beyond the parsed map. A missing file is created with commented defaults
//! so the options are discoverable.

use std::collections::HashMap;
use std::path::PathBuf;

// MOD_CONTROL / VK_ESCAPE as plain integers to keep this module POD.
pub const DEFAULT_HOTKEY_MODS: u32 = 0x0002;
pub const DEFAULT_HOTKEY_VK: u32 = 0x1B;

const DEFAULT_FILE: &str = r#"# Apex configuration
# Changes take effect on restart.

[general]
# Create/refresh a Start menu entry for apex on launch.
start_menu = true
# Launch apex automatically at sign-in.
start_on_startup = true

[hotkey]
# Modifiers joined with '+': ctrl, alt, shift, win. Use "none" for bare keys.
modifiers = "ctrl"
# Key: a-z, 0-9, f1-f24, space, escape, tab, grave, enter
key = "escape"

[plugins]
# Set to false to disable a plugin entirely; a disabled plugin is never
# constructed and uses zero memory.
search = true
"#;

pub struct Config {
    values: HashMap<(String, String), String>,
    pub hotkey_mods: u32,
    pub hotkey_vk: u32,
}

impl Config {
    pub fn load() -> Self {
        let text = read_or_create().unwrap_or_default();
        Self::from_text(&text)
    }

    fn from_text(text: &str) -> Self {
        let values = parse(text);
        let (hotkey_mods, hotkey_vk) =
            hotkey_from(&values).unwrap_or((DEFAULT_HOTKEY_MODS, DEFAULT_HOTKEY_VK));
        Self {
            values,
            hotkey_mods,
            hotkey_vk,
        }
    }

    /// Plugins default to enabled when not mentioned in the file.
    pub fn plugin_enabled(&self, id: &str) -> bool {
        match self.values.get(&("plugins".to_string(), id.to_lowercase())) {
            Some(v) => v.eq_ignore_ascii_case("true"),
            None => true,
        }
    }

    /// Boolean from the `[general]` section with a default.
    pub fn general_flag(&self, key: &str, default: bool) -> bool {
        match self.values.get(&("general".to_string(), key.to_string())) {
            Some(v) => v.eq_ignore_ascii_case("true"),
            None => default,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("apex").join("config.toml"))
}

fn read_or_create() -> Option<String> {
    let path = config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, DEFAULT_FILE);
            Some(DEFAULT_FILE.to_string())
        }
    }
}

fn parse(text: &str) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_lowercase();
        } else if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_lowercase();
            let val = unquote(v.trim()).to_string();
            out.insert((section.clone(), key), val);
        }
    }
    out
}

/// Cut a `#` comment, respecting double-quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(v: &str) -> &str {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
}

fn hotkey_from(values: &HashMap<(String, String), String>) -> Option<(u32, u32)> {
    let mods = values.get(&("hotkey".to_string(), "modifiers".to_string()))?;
    let key = values.get(&("hotkey".to_string(), "key".to_string()))?;
    Some((parse_mods(mods)?, parse_key(key)?))
}

fn parse_mods(s: &str) -> Option<u32> {
    let s = s.trim().to_lowercase();
    if s == "none" {
        return Some(0);
    }
    let mut mods = 0u32;
    for part in s.split('+') {
        mods |= match part.trim() {
            "alt" => 0x1,
            "ctrl" | "control" => 0x2,
            "shift" => 0x4,
            "win" | "super" => 0x8,
            _ => return None,
        };
    }
    Some(mods)
}

fn parse_key(s: &str) -> Option<u32> {
    let s = s.trim().to_lowercase();
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ 'a'..='z'), None) => return Some(0x41 + (c as u32 - 'a' as u32)),
        (Some(c @ '0'..='9'), None) => return Some(0x30 + (c as u32 - '0' as u32)),
        _ => {}
    }
    if let Some(n) = s.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    match s.as_str() {
        "space" => Some(0x20),
        "escape" | "esc" => Some(0x1B),
        "tab" => Some(0x09),
        "enter" | "return" => Some(0x0D),
        "grave" | "backtick" | "`" => Some(0xC0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_comments_quotes() {
        let cfg = Config::from_text(
            "# comment\n[Hotkey]\nmodifiers = \"ctrl+alt\" # trailing\nkey = f19\n\n[plugins]\nsearch = false\n",
        );
        assert_eq!(cfg.hotkey_mods, 0x2 | 0x1);
        assert_eq!(cfg.hotkey_vk, 0x70 + 18);
        assert!(!cfg.plugin_enabled("search"));
        assert!(cfg.plugin_enabled("unlisted"));
    }

    #[test]
    fn defaults_when_missing_or_invalid() {
        for text in ["", "[hotkey]\nmodifiers = \"bogus\"\nkey = \"escape\"\n"] {
            let cfg = Config::from_text(text);
            assert_eq!(cfg.hotkey_mods, DEFAULT_HOTKEY_MODS);
            assert_eq!(cfg.hotkey_vk, DEFAULT_HOTKEY_VK);
        }
    }

    #[test]
    fn default_file_round_trips() {
        let cfg = Config::from_text(DEFAULT_FILE);
        assert_eq!(cfg.hotkey_mods, DEFAULT_HOTKEY_MODS);
        assert_eq!(cfg.hotkey_vk, DEFAULT_HOTKEY_VK);
        assert!(cfg.plugin_enabled("search"));
        assert!(cfg.general_flag("start_menu", true));
        assert!(cfg.general_flag("start_on_startup", true));
    }

    #[test]
    fn general_flags() {
        let cfg = Config::from_text("[general]\nstart_menu = false\n");
        assert!(!cfg.general_flag("start_menu", true));
        assert!(cfg.general_flag("start_on_startup", true));
        assert!(!cfg.general_flag("unknown", false));
    }

    #[test]
    fn key_names() {
        assert_eq!(parse_key("a"), Some(0x41));
        assert_eq!(parse_key("Z"), Some(0x5A));
        assert_eq!(parse_key("7"), Some(0x37));
        assert_eq!(parse_key("f1"), Some(0x70));
        assert_eq!(parse_key("f24"), Some(0x87));
        assert_eq!(parse_key("f25"), None);
        assert_eq!(parse_key("space"), Some(0x20));
        assert_eq!(parse_key("nope"), None);
    }

    #[test]
    fn modifier_combos() {
        assert_eq!(parse_mods("ctrl"), Some(0x2));
        assert_eq!(parse_mods("Ctrl+Shift"), Some(0x6));
        assert_eq!(parse_mods("win+alt"), Some(0x9));
        assert_eq!(parse_mods("none"), Some(0));
        assert_eq!(parse_mods("hyper"), None);
    }
}
