//! The composer keymap: `[ui.keymap]` in the user config remaps the
//! session's interactive keys, the way a user's hands know them.
//!
//! Remappable actions are the ones with a natural other home: the editor
//! compose (default Ctrl+G), the draft stash (Ctrl+S), the
//! background demotion (Ctrl+B), and the `@`-mention completion (Tab).
//! Ctrl+C is not remappable — it is the universal interrupt, and a
//! keymap that could take it away would be a footgun with one entry.
//!
//! The keymap loads once per process from the resolved user config; a
//! change takes effect on the next start, like every other session-start
//! setting. Keys name as `ctrl+x`, `alt+x`, `f1`…`f12`, `tab`, `enter`,
//! `esc`, or a single character.

use std::path::Path;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::OnceLock;

/// One remappable action.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Action {
    /// Compose the draft in `$VISUAL`/`$EDITOR` (default `ctrl+g`).
    Editor,
    /// Stash the draft, or bring the stash back (default `ctrl+s`).
    Stash,
    /// Move the foreground command to the background (default `ctrl+b`).
    Background,
    /// Complete the trailing `@`-mention (default `tab`).
    Mention,
}

impl Action {
    pub const ALL: [Self; 4] = [Self::Editor, Self::Stash, Self::Background, Self::Mention];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Editor => "editor_key",
            Self::Stash => "stash_key",
            Self::Background => "background_key",
            Self::Mention => "mention_key",
        }
    }

    pub const fn default_key(self) -> Key {
        match self {
            Self::Editor => Key::Ctrl('g'),
            Self::Stash => Key::Ctrl('s'),
            Self::Background => Key::Ctrl('b'),
            Self::Mention => Key::Tab,
        }
    }
}

/// One parsed key: the modifiers a crossterm key event can carry here,
/// plus the key itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Key {
    Ctrl(char),
    Alt(char),
    Function(u8),
    Tab,
    Enter,
    Esc,
    Char(char),
}

impl Key {
    /// Parse `ctrl+x`, `alt+x`, `f1`..`f12`, `tab`, `enter`, `esc`, or a
    /// single character (case-insensitive for letters: a keymap names
    /// keys, and terminals deliver letters case-insensitively to us).
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        let (modifier, rest) = match raw.split_once('+') {
            Some(("ctrl", rest)) => (Some(0), rest),
            Some(("alt", rest)) => (Some(1), rest),
            _ => (None, raw.as_str()),
        };
        if rest.is_empty() || rest.contains('+') {
            return None;
        }
        // A modifier only combines with a single character — `ctrl+tab`
        // names nothing this build reads.
        if modifier.is_some() && rest.chars().count() != 1 {
            return None;
        }
        let mut chars = rest.chars();
        let key = match (chars.next(), chars.next()) {
            (Some(one), None) => one,
            _ => {
                // A multi-character name: f1..f12, tab, enter, esc.
                return match rest {
                    "tab" => Some(Key::Tab),
                    "enter" | "return" => Some(Key::Enter),
                    "esc" | "escape" => Some(Key::Esc),
                    other => other
                        .strip_prefix('f')
                        .and_then(|n| n.parse::<u8>().ok())
                        .filter(|n| (1..=12).contains(n))
                        .map(Key::Function),
                };
            }
        };
        if !key.is_ascii_alphanumeric() {
            return None;
        }
        match modifier {
            Some(0) => Some(Key::Ctrl(key)),
            Some(1) => Some(Key::Alt(key)),
            _ => Some(Key::Char(key)),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ctrl(ch) => match ch {
                'g' => "ctrl+g",
                's' => "ctrl+s",
                'b' => "ctrl+b",
                _ => "ctrl+?",
            },
            Self::Alt(_) => "alt+?",
            Self::Function(n) => match n {
                1 => "f1",
                2 => "f2",
                3 => "f3",
                4 => "f4",
                5 => "f5",
                6 => "f6",
                7 => "f7",
                8 => "f8",
                9 => "f9",
                10 => "f10",
                11 => "f11",
                _ => "f12",
            },
            Self::Tab => "tab",
            Self::Enter => "enter",
            Self::Esc => "esc",
            Self::Char('a') => "a",
            Self::Char(_) => "?",
        }
    }
}

/// The session's keymap: every remappable action's key, defaults filled
/// in for anything the config does not name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Keymap {
    keys: [Key; 4],
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            keys: [
                Action::Editor.default_key(),
                Action::Stash.default_key(),
                Action::Background.default_key(),
                Action::Mention.default_key(),
            ],
        }
    }
}

impl Keymap {
    /// The key bound to `action`.
    pub fn key(&self, action: Action) -> Key {
        self.keys[action as usize]
    }

    /// Which action, if any, a key performs under this keymap.
    pub fn action(&self, key: Key) -> Option<Action> {
        Action::ALL
            .into_iter()
            .zip(self.keys)
            .find(|(_, bound)| *bound == key)
            .map(|(action, _)| action)
    }

    /// Parse the `[ui.keymap]` table (pure): every key must parse, no two
    /// actions may share a key (a collision would silently shadow one),
    /// and an unknown action name is refused rather than ignored.
    pub fn parse(table: &toml::Value) -> Result<Self, String> {
        let mut keymap = Self::default();
        let entries = table
            .as_table()
            .ok_or_else(|| "[ui.keymap] must be a table".to_owned())?;
        for (name, value) in entries {
            let action = Action::ALL
                .into_iter()
                .find(|action| action.as_str() == name)
                .ok_or_else(|| format!("unknown keymap action {name:?}"))?;
            let raw = value
                .as_str()
                .ok_or_else(|| format!("{name} must be a key name string"))?;
            let key =
                Key::parse(raw).ok_or_else(|| format!("{name}: {raw:?} is not a key name"))?;
            keymap.keys[action as usize] = key;
        }
        // Collisions: two actions on one key silently shadows one — the
        // last-write-wins of a naive map, refused here instead.
        for (index, key) in keymap.keys.iter().enumerate() {
            if keymap.keys[..index].contains(key) {
                let first = Action::ALL
                    .into_iter()
                    .zip(keymap.keys)
                    .find(|(_, bound)| bound == key)
                    .map(|(action, _)| action.as_str())
                    .unwrap_or_default();
                let second = Action::ALL[index].as_str();
                return Err(format!(
                    "{name_of}: two actions share the key {} ({first} and {second})",
                    key.as_str(),
                    name_of = "[ui.keymap]",
                ));
            }
        }
        Ok(keymap)
    }
}

/// The process keymap: loaded once from the user config beside
/// `config.toml`'s home (the same file `[ui.status_line]` lives in), then
/// fixed — a change takes effect on the next start, like every other
/// session-start setting.
static ACTIVE: OnceLock<Keymap> = OnceLock::new();
#[cfg(test)]
static TEST_OVERRIDE: Mutex<Option<Keymap>> = Mutex::new(None);

/// The keymap this process runs under: the test override when one is set,
/// else the once-loaded configuration.
pub fn active() -> Keymap {
    #[cfg(test)]
    {
        if let Some(overridden) = TEST_OVERRIDE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return overridden;
        }
    }
    ACTIVE.get_or_init(load_default).clone()
}

fn load_default() -> Keymap {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let source = crate::user_config::resolve_config_source(&env);
    let path = match source {
        crate::user_config::ConfigSource::ExplicitPath(path)
        | crate::user_config::ConfigSource::HomeFallback(path) => path,
    };
    load_from(&path).unwrap_or_default()
}

/// Load `[ui.keymap]` from one config file (pure read + parse): missing
/// section means defaults; a broken one is defaults with the reason said.
pub fn load_from(path: &Path) -> Result<Keymap, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // Absent is the ordinary case: the defaults, no error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Keymap::default()),
        Err(err) => return Err(err.to_string()),
    };
    let parsed: toml::Value = toml::from_str(&text).map_err(|err| err.to_string())?;
    match parsed.get("ui").and_then(|ui| ui.get("keymap")) {
        None => Ok(Keymap::default()),
        Some(table) => Keymap::parse(table),
    }
}

/// Test-only: pin the process keymap (the caller restores the default).
#[cfg(test)]
pub(crate) fn set_test_keymap(keymap: Option<Keymap>) {
    *TEST_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = keymap;
}

/// The session-start notice path used by production: load once, say why
/// when the config's table could not be honoured, and pin it.
pub fn install(notify: &mut dyn FnMut(&str)) -> Keymap {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let source = crate::user_config::resolve_config_source(&env);
    let path = match source {
        crate::user_config::ConfigSource::ExplicitPath(path)
        | crate::user_config::ConfigSource::HomeFallback(path) => path,
    };
    let loaded = match load_from(&path) {
        Ok(keymap) => keymap,
        Err(reason) => {
            notify(&format!(
                "[ui.keymap] could not be applied ({reason}); the default keys stand"
            ));
            Keymap::default()
        }
    };
    let _ = ACTIVE.set(loaded.clone());
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_in_every_documented_shape() {
        assert_eq!(Key::parse("ctrl+g"), Some(Key::Ctrl('g')));
        assert_eq!(Key::parse("alt+u"), Some(Key::Alt('u')));
        assert_eq!(
            Key::parse("G"),
            Some(Key::Char('g')),
            "case-insensitive letters"
        );
        assert_eq!(Key::parse("tab"), Some(Key::Tab));
        assert_eq!(Key::parse("enter"), Some(Key::Enter));
        assert_eq!(Key::parse("esc"), Some(Key::Esc));
        assert_eq!(Key::parse("f5"), Some(Key::Function(5)));
        assert_eq!(Key::parse("k"), Some(Key::Char('k')));
        // Everything that cannot be a key name is None, never a guess.
        for bad in ["", "ctrl+", "++", "f0", "f13", "fx", "ctrl+tab", "no"] {
            assert!(Key::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_keymap_remaps_actions_and_refuses_collisions() {
        let table: toml::Value = toml::from_str(
            r#"
editor_key = "ctrl+k"
mention_key = "f2"
"#,
        )
        .expect("toml");
        let keymap = Keymap::parse(&table).expect("keymap");
        assert_eq!(keymap.key(Action::Editor), Key::Ctrl('k'));
        assert_eq!(keymap.key(Action::Mention), Key::Function(2));
        assert_eq!(keymap.key(Action::Stash), Key::Ctrl('s'), "default stands");
        assert_eq!(keymap.action(Key::Ctrl('k')), Some(Action::Editor));
        assert_eq!(keymap.action(Key::Function(2)), Some(Action::Mention));
        assert_eq!(keymap.action(Key::Ctrl('s')), Some(Action::Stash));
        assert_eq!(keymap.action(Key::Ctrl('x')), None);

        // Two actions on one key are refused naming both.
        let collision: toml::Value =
            toml::from_str("editor_key = \"ctrl+k\"\nstash_key = \"ctrl+k\"").expect("toml");
        let err = Keymap::parse(&collision).expect_err("collision");
        assert!(err.contains("share the key"), "{err}");

        // Unknown action names and unparseable keys are refused.
        let unknown: toml::Value = toml::from_str(r#"quit_key = "ctrl+x""#).expect("toml");
        assert!(Keymap::parse(&unknown).is_err());
        let bad_key: toml::Value = toml::from_str(r#"editor_key = "nonsense""#).expect("toml");
        assert!(Keymap::parse(&bad_key).is_err());
    }

    #[test]
    fn a_config_files_keymap_loads_and_a_missing_section_means_defaults() {
        let dir = std::env::temp_dir().join(format!("rapidlm-keymap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("config.toml");

        // Missing file: defaults.
        assert_eq!(load_from(&config).expect("defaults"), Keymap::default());

        std::fs::write(&config, "[ui.keymap]\neditor_key = \"f9\"\n").expect("write");
        let keymap = load_from(&config).expect("loaded");
        assert_eq!(keymap.key(Action::Editor), Key::Function(9));

        // A broken table falls to defaults with the reason named.
        std::fs::write(&config, "[ui.keymap]\neditor_key = \"not a key\"\n").expect("write");
        let err = load_from(&config).expect_err("broken");
        assert!(err.contains("not a key name"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
