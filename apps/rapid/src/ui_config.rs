//! The user's interface preferences: a colour theme and key bindings.
//!
//! Read from `[ui.theme]` and `[ui.keys]` in the user's own `config.toml` and
//! nowhere else. A project may not restyle or rebind someone's terminal: the
//! worst a checked-in preference can do is hide a warning or move interrupt
//! somewhere surprising.
//!
//! ```toml
//! [ui.theme]
//! name = "dark"                 # default, dark, light, mono
//!
//! [ui.theme.styles]             # optional per-role overrides
//! status = "black on cyan"
//! error  = "bold #ff5555"
//!
//! [ui.keys]
//! stash       = "ctrl-x"
//! toggle_plan = ["shift-tab", "alt-p"]
//! page_up     = "alt-up"
//! ```
//!
//! Anything that does not parse is dropped on its own and reported, and the
//! rest of the file still applies: a typo in one colour must not cost the
//! person every other setting.

use std::collections::BTreeMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui::theme::{Role, Style, Theme};

use crate::interactive::InteractiveInput;

/// What a key press can do.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Action {
    /// Stop the running turn; twice, leave.
    Interrupt,
    /// Set the draft aside, or bring it back.
    Stash,
    /// Move the foreground command to the background.
    Background,
    /// Complete the `@path` or `/command`; on an empty prompt, switch plan mode.
    Complete,
    /// Switch between plan and build.
    TogglePlan,
    /// Scroll the transcript toward earlier output.
    PageUp,
    /// Scroll the transcript toward later output.
    PageDown,
}

impl Action {
    /// Every action, in the order a listing shows them.
    pub const ALL: [Self; 7] = [
        Self::Interrupt,
        Self::Stash,
        Self::Background,
        Self::Complete,
        Self::TogglePlan,
        Self::PageUp,
        Self::PageDown,
    ];

    /// The name used in configuration.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Stash => "stash",
            Self::Background => "background",
            Self::Complete => "complete",
            Self::TogglePlan => "toggle_plan",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }

    fn input(self) -> InteractiveInput {
        match self {
            Self::Interrupt => InteractiveInput::CtrlC,
            Self::Stash => InteractiveInput::CtrlS,
            Self::Background => InteractiveInput::CtrlB,
            Self::Complete => InteractiveInput::Tab,
            Self::TogglePlan => InteractiveInput::BackTab,
            Self::PageUp => InteractiveInput::PageUp,
            Self::PageDown => InteractiveInput::PageDown,
        }
    }

    fn defaults(self) -> &'static [&'static str] {
        match self {
            Self::Interrupt => &["ctrl-c"],
            Self::Stash => &["ctrl-s"],
            Self::Background => &["ctrl-b"],
            Self::Complete => &["tab"],
            Self::TogglePlan => &["shift-tab"],
            Self::PageUp => &["pageup"],
            Self::PageDown => &["pagedown"],
        }
    }
}

/// One key with its modifiers, normalised.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Chord {
    code: ChordKey,
    ctrl: bool,
    alt: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ChordKey {
    /// A printable key, lower-cased.
    Char(char),
    Tab,
    /// Shift and Tab together: terminals report it as its own key.
    BackTab,
    Esc,
    PageUp,
    PageDown,
    Home,
    End,
    Up,
    Down,
    Left,
    Right,
    Function(u8),
}

impl Chord {
    /// Parse `ctrl-x`, `alt-up`, `shift-tab`, `f5`, `pageup`.
    ///
    /// # Errors
    /// The part that is not a modifier or a key, or a binding that would take
    /// over typing (a bare letter) or that the terminal cannot tell from
    /// another key (`ctrl-i` is Tab, `ctrl-m` is Enter).
    pub fn parse(text: &str) -> Result<Self, String> {
        let lowered = text.trim().to_ascii_lowercase();
        let mut parts: Vec<&str> = lowered.split('-').collect();
        // A literal minus is `ctrl--`: the split leaves two empty parts.
        let key_name = if lowered.ends_with("--") || lowered == "-" {
            parts.truncate(parts.len().saturating_sub(2));
            "-"
        } else {
            parts.pop().unwrap_or("")
        };
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        for modifier in parts {
            match modifier {
                "ctrl" | "control" | "c" => ctrl = true,
                "alt" | "meta" | "opt" | "option" | "a" | "m" => alt = true,
                "shift" | "s" => shift = true,
                other => return Err(format!("`{other}` is not a modifier (ctrl, alt, shift)")),
            }
        }
        let code = match key_name {
            "tab" if shift => ChordKey::BackTab,
            "tab" => ChordKey::Tab,
            "esc" | "escape" => ChordKey::Esc,
            "pageup" | "pgup" => ChordKey::PageUp,
            "pagedown" | "pgdn" | "pgdown" => ChordKey::PageDown,
            "home" => ChordKey::Home,
            "end" => ChordKey::End,
            "up" => ChordKey::Up,
            "down" => ChordKey::Down,
            "left" => ChordKey::Left,
            "right" => ChordKey::Right,
            "enter" | "return" | "backspace" | "space" => {
                return Err(format!(
                    "`{key_name}` cannot be rebound: it is how you type"
                ));
            }
            f if f
                .strip_prefix('f')
                .is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n))) =>
            {
                ChordKey::Function(f[1..].parse().unwrap_or(1))
            }
            single if single.chars().count() == 1 => {
                ChordKey::Char(single.chars().next().unwrap_or(' '))
            }
            "" => return Err("no key".to_owned()),
            other => return Err(format!("`{other}` is not a key I know")),
        };
        if shift && !matches!(code, ChordKey::BackTab) {
            // Shift changes what a letter means and most terminals do not
            // report it for other keys; only shift-tab is distinct.
            return Err("shift is only supported with tab (shift-tab)".to_owned());
        }
        if let ChordKey::Char(c) = code {
            if !ctrl && !alt {
                return Err(format!(
                    "a bare `{c}` would take over typing; add ctrl- or alt-"
                ));
            }
            if ctrl && matches!(c, 'i' | 'm' | 'h' | 'j' | '[') {
                return Err(format!(
                    "ctrl-{c} is indistinguishable from {} in a terminal",
                    match c {
                        'i' => "tab",
                        'm' | 'j' => "enter",
                        'h' => "backspace",
                        _ => "escape",
                    }
                ));
            }
        }
        Ok(Self { code, ctrl, alt })
    }

    /// Whether `key` is this chord.
    #[must_use]
    pub fn matches(&self, key: &KeyEvent) -> bool {
        let code = match key.code {
            KeyCode::Char(c) => ChordKey::Char(c.to_ascii_lowercase()),
            KeyCode::Tab => ChordKey::Tab,
            KeyCode::BackTab => ChordKey::BackTab,
            KeyCode::Esc => ChordKey::Esc,
            KeyCode::PageUp => ChordKey::PageUp,
            KeyCode::PageDown => ChordKey::PageDown,
            KeyCode::Home => ChordKey::Home,
            KeyCode::End => ChordKey::End,
            KeyCode::Up => ChordKey::Up,
            KeyCode::Down => ChordKey::Down,
            KeyCode::Left => ChordKey::Left,
            KeyCode::Right => ChordKey::Right,
            KeyCode::F(n) => ChordKey::Function(n),
            _ => return false,
        };
        code == self.code
            && key.modifiers.contains(KeyModifiers::CONTROL) == self.ctrl
            && key.modifiers.contains(KeyModifiers::ALT) == self.alt
    }

    /// The chord as it is written in configuration.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("ctrl-");
        }
        if self.alt {
            out.push_str("alt-");
        }
        out.push_str(&match self.code {
            ChordKey::Char(c) => c.to_string(),
            ChordKey::Tab => "tab".to_owned(),
            ChordKey::BackTab => "shift-tab".to_owned(),
            ChordKey::Esc => "esc".to_owned(),
            ChordKey::PageUp => "pageup".to_owned(),
            ChordKey::PageDown => "pagedown".to_owned(),
            ChordKey::Home => "home".to_owned(),
            ChordKey::End => "end".to_owned(),
            ChordKey::Up => "up".to_owned(),
            ChordKey::Down => "down".to_owned(),
            ChordKey::Left => "left".to_owned(),
            ChordKey::Right => "right".to_owned(),
            ChordKey::Function(n) => format!("f{n}"),
        });
        out
    }
}

/// Which keys do what.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyMap {
    bindings: BTreeMap<Action, Vec<Chord>>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self {
            bindings: Action::ALL
                .into_iter()
                .map(|action| {
                    let chords = action
                        .defaults()
                        .iter()
                        .filter_map(|text| Chord::parse(text).ok())
                        .collect();
                    (action, chords)
                })
                .collect(),
        }
    }
}

impl KeyMap {
    /// The input a key press stands for, if it is bound.
    #[must_use]
    pub fn lookup(&self, key: &KeyEvent) -> Option<InteractiveInput> {
        self.bindings
            .iter()
            .find(|(_, chords)| chords.iter().any(|chord| chord.matches(key)))
            .map(|(action, _)| action.input())
    }

    /// The chords bound to `action`.
    #[must_use]
    pub fn chords(&self, action: Action) -> &[Chord] {
        self.bindings.get(&action).map_or(&[], Vec::as_slice)
    }

    fn rebind(&mut self, action: Action, chords: Vec<Chord>) -> Result<(), String> {
        let mut chords = chords;
        chords.dedup();
        if action == Action::Interrupt {
            // Whatever else is chosen, Ctrl-C still stops a runaway turn.
            if let Ok(always) = Chord::parse("ctrl-c")
                && !chords.contains(&always)
            {
                chords.push(always);
            }
        }
        for chord in &chords {
            if let Some((other, _)) = self
                .bindings
                .iter()
                .find(|(other, held)| **other != action && held.contains(chord))
            {
                return Err(format!(
                    "{} is already {}; unbind that first",
                    chord.describe(),
                    other.name()
                ));
            }
        }
        self.bindings.insert(action, chords);
        Ok(())
    }

    /// One line per action, as `/keys` shows them.
    #[must_use]
    pub fn listing(&self) -> String {
        Action::ALL
            .into_iter()
            .map(|action| {
                let chords = self.chords(action);
                let keys = if chords.is_empty() {
                    "(unbound)".to_owned()
                } else {
                    chords
                        .iter()
                        .map(Chord::describe)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("{:<12} {keys}", action.name())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the user's config asks for.
#[derive(Clone, Debug, Default)]
pub struct UiConfig {
    pub theme: Option<Theme>,
    pub keys: KeyMap,
    /// Entries that were dropped, and why.
    pub problems: Vec<String>,
}

/// Read the preferences from the user's config file. A file that cannot be read
/// is no preferences; one that does not parse as TOML is reported and also none.
#[must_use]
pub fn load(user_config: &Path) -> UiConfig {
    match std::fs::read_to_string(user_config) {
        Ok(text) => parse(&text, &user_config.display().to_string()),
        Err(_) => UiConfig::default(),
    }
}

/// Parse the preferences out of a config document.
#[must_use]
pub fn parse(text: &str, origin: &str) -> UiConfig {
    let mut config = UiConfig::default();
    let doc: toml::Table = match text.parse() {
        Ok(doc) => doc,
        Err(err) => {
            config.problems.push(format!(
                "{origin}: not valid TOML, interface preferences ignored ({err})"
            ));
            return config;
        }
    };
    let Some(ui) = doc.get("ui").and_then(toml::Value::as_table) else {
        return config;
    };
    if let Some(theme) = ui.get("theme").and_then(toml::Value::as_table) {
        config.theme = parse_theme(theme, &mut config.problems);
    }
    if let Some(keys) = ui.get("keys").and_then(toml::Value::as_table) {
        parse_keys(keys, &mut config);
    }
    config
}

fn parse_theme(table: &toml::Table, problems: &mut Vec<String>) -> Option<Theme> {
    let name = table
        .get("name")
        .and_then(toml::Value::as_str)
        .unwrap_or("default");
    let mut theme = Theme::builtin(name).unwrap_or_else(|| {
        problems.push(format!(
            "ui.theme.name: `{name}` is not a theme ({}); using default",
            Theme::BUILTIN.join(", ")
        ));
        Theme::plain()
    });
    if let Some(styles) = table.get("styles").and_then(toml::Value::as_table) {
        for (role_name, value) in styles {
            let Some(role) = Role::from_name(role_name) else {
                let roles: Vec<&str> = Role::ALL.iter().map(|r| r.name()).collect();
                problems.push(format!(
                    "ui.theme.styles.{role_name}: not a role ({})",
                    roles.join(", ")
                ));
                continue;
            };
            let Some(spec) = value.as_str() else {
                problems.push(format!("ui.theme.styles.{role_name}: expected a string"));
                continue;
            };
            match Style::parse(spec) {
                Ok(style) => theme.set(role, style),
                Err(why) => problems.push(format!("ui.theme.styles.{role_name}: {why}")),
            }
        }
    }
    Some(theme)
}

fn parse_keys(table: &toml::Table, config: &mut UiConfig) {
    for (name, value) in table {
        let Some(action) = Action::from_name(name) else {
            let names: Vec<&str> = Action::ALL.iter().map(|a| a.name()).collect();
            config.problems.push(format!(
                "ui.keys.{name}: not an action ({})",
                names.join(", ")
            ));
            continue;
        };
        let words: Vec<&str> = match value {
            toml::Value::String(one) => vec![one.as_str()],
            toml::Value::Array(many) => many.iter().filter_map(toml::Value::as_str).collect(),
            _ => {
                config
                    .problems
                    .push(format!("ui.keys.{name}: expected a key or a list of keys"));
                continue;
            }
        };
        let mut chords = Vec::new();
        let mut ok = true;
        for word in words {
            match Chord::parse(word) {
                Ok(chord) => chords.push(chord),
                Err(why) => {
                    config
                        .problems
                        .push(format!("ui.keys.{name}: `{word}`: {why}"));
                    ok = false;
                }
            }
        }
        // One bad key drops the whole entry rather than half-applying it.
        if ok && let Err(why) = config.keys.rebind(action, chords) {
            config.problems.push(format!("ui.keys.{name}: {why}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn chords_parse_in_the_forms_people_write() {
        for (text, shown) in [
            ("ctrl-x", "ctrl-x"),
            ("CTRL-X", "ctrl-x"),
            ("alt-up", "alt-up"),
            ("ctrl-alt-p", "ctrl-alt-p"),
            ("shift-tab", "shift-tab"),
            ("tab", "tab"),
            ("f5", "f5"),
            ("pgup", "pageup"),
            ("ctrl--", "ctrl--"),
            ("meta-b", "alt-b"),
        ] {
            assert_eq!(Chord::parse(text).expect(text).describe(), shown, "{text}");
        }
    }

    #[test]
    fn chords_that_would_break_typing_or_cannot_be_told_apart_are_refused() {
        for text in [
            "a",
            "x",
            "enter",
            "backspace",
            "space",
            "ctrl-i",
            "ctrl-m",
            "ctrl-h",
            "ctrl-[",
            "shift-x",
            "ctrl-",
            "",
            "f13",
            "hyper-x",
            "ctrl-nonsense",
        ] {
            assert!(Chord::parse(text).is_err(), "{text:?} should be refused");
        }
    }

    #[test]
    fn a_chord_matches_the_key_the_terminal_reports() {
        let ctrl_s = Chord::parse("ctrl-s").unwrap();
        assert!(ctrl_s.matches(&key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        assert!(ctrl_s.matches(&key(
            KeyCode::Char('S'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!ctrl_s.matches(&key(KeyCode::Char('s'), KeyModifiers::NONE)));
        assert!(!ctrl_s.matches(&key(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        let back_tab = Chord::parse("shift-tab").unwrap();
        assert!(back_tab.matches(&key(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert!(!back_tab.matches(&key(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(
            Chord::parse("alt-up")
                .unwrap()
                .matches(&key(KeyCode::Up, KeyModifiers::ALT))
        );
    }

    #[test]
    fn the_defaults_are_the_keys_the_program_has_always_had() {
        let keys = KeyMap::default();
        for (code, mods, input) in [
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                InteractiveInput::CtrlC,
            ),
            (
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
                InteractiveInput::CtrlS,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                InteractiveInput::CtrlB,
            ),
            (KeyCode::Tab, KeyModifiers::NONE, InteractiveInput::Tab),
            (
                KeyCode::BackTab,
                KeyModifiers::SHIFT,
                InteractiveInput::BackTab,
            ),
            (
                KeyCode::PageUp,
                KeyModifiers::NONE,
                InteractiveInput::PageUp,
            ),
            (
                KeyCode::PageDown,
                KeyModifiers::NONE,
                InteractiveInput::PageDown,
            ),
        ] {
            assert_eq!(keys.lookup(&key(code, mods)), Some(input));
        }
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn a_rebinding_replaces_the_default_and_a_list_binds_several() {
        let config = parse(
            "[ui.keys]\nstash = \"ctrl-x\"\ntoggle_plan = [\"shift-tab\", \"alt-p\"]\npage_up = \"alt-up\"\n",
            "test",
        );
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let keys = &config.keys;
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some(InteractiveInput::CtrlS)
        );
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            None,
            "the old key is free"
        );
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('p'), KeyModifiers::ALT)),
            Some(InteractiveInput::BackTab)
        );
        assert_eq!(
            keys.lookup(&key(KeyCode::Up, KeyModifiers::ALT)),
            Some(InteractiveInput::PageUp)
        );
        assert_eq!(keys.lookup(&key(KeyCode::PageUp, KeyModifiers::NONE)), None);
    }

    #[test]
    fn interrupt_always_keeps_ctrl_c() {
        let config = parse("[ui.keys]\ninterrupt = \"ctrl-q\"\n", "test");
        let keys = &config.keys;
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(InteractiveInput::CtrlC)
        );
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InteractiveInput::CtrlC),
            "a runaway turn can always be stopped"
        );
        // Another action may not take it.
        let taken = parse("[ui.keys]\nstash = \"ctrl-c\"\n", "test");
        assert!(
            taken.problems[0].contains("already interrupt"),
            "{:?}",
            taken.problems
        );
    }

    #[test]
    fn a_clash_or_a_bad_key_drops_that_entry_and_keeps_the_rest() {
        let config = parse(
            "[ui.keys]\ncomplete = \"pageup\"\nbackground = \"ctrl-z\"\npage_up = [\"alt-up\", \"x\"]\nnonsense = \"ctrl-q\"\npage_down = 3\n",
            "test",
        );
        let said = config.problems.join("\n");
        assert!(
            said.contains("complete") && said.contains("already page_up"),
            "{said}"
        );
        assert!(
            said.contains("page_up") && said.contains("bare `x`"),
            "{said}"
        );
        assert!(said.contains("nonsense: not an action"), "{said}");
        assert!(said.contains("page_down: expected"), "{said}");
        let keys = &config.keys;
        // Entries apply independently: background moved, stash is untouched, and
        // `complete`'s clash with page_up's key was refused.
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(InteractiveInput::CtrlB)
        );
        assert_eq!(
            keys.lookup(&key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(InteractiveInput::CtrlS)
        );
        // page_up's entry was dropped whole, so PageUp still works.
        assert_eq!(
            keys.lookup(&key(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(InteractiveInput::PageUp)
        );
        assert_eq!(keys.lookup(&key(KeyCode::Up, KeyModifiers::ALT)), None);
    }

    #[test]
    fn an_action_can_be_unbound_with_an_empty_list() {
        let config = parse("[ui.keys]\nbackground = []\n", "test");
        assert!(config.problems.is_empty());
        assert_eq!(
            config
                .keys
                .lookup(&key(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            None
        );
        assert!(config.keys.listing().contains("background   (unbound)"));
    }

    #[test]
    fn a_theme_names_a_built_in_and_overrides_roles() {
        let config = parse(
            "[ui.theme]\nname = \"dark\"\n[ui.theme.styles]\nstatus = \"black on cyan\"\nerror = \"bold #ff5555\"\n",
            "test",
        );
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let theme = config.theme.expect("theme");
        assert_eq!(theme.name(), "dark");
        assert!(theme.style(Role::Status).unwrap().bg.is_some());
        assert!(
            theme.style(Role::Tool).unwrap().dim,
            "an untouched role keeps the built-in's style"
        );
    }

    #[test]
    fn a_bad_theme_entry_is_reported_and_the_rest_applies() {
        let config = parse(
            "[ui.theme]\nname = \"neon\"\n[ui.theme.styles]\nstatus = \"loud\"\nbogus = \"red\"\nerror = 5\nuser = \"bold\"\n",
            "test",
        );
        let said = config.problems.join("\n");
        assert!(said.contains("`neon` is not a theme"), "{said}");
        assert!(said.contains("status") && said.contains("`loud`"), "{said}");
        assert!(said.contains("bogus: not a role"), "{said}");
        assert!(said.contains("error: expected a string"), "{said}");
        let theme = config.theme.expect("theme");
        assert!(
            theme.style(Role::User).unwrap().bold,
            "the good entry still applied"
        );
        assert!(theme.style(Role::Status).is_none());
    }

    #[test]
    fn no_ui_table_means_no_preferences_and_bad_toml_is_reported_once() {
        let none = parse("[models]\ndefault = \"x\"\n", "test");
        assert!(none.theme.is_none() && none.problems.is_empty());
        assert_eq!(none.keys, KeyMap::default());
        let broken = parse("[ui\nname=", "test");
        assert!(broken.theme.is_none());
        assert_eq!(broken.problems.len(), 1);
        assert!(broken.problems[0].contains("not valid TOML"));
    }

    #[test]
    fn a_missing_file_is_no_preferences() {
        let config = load(Path::new("/definitely/not/here/config.toml"));
        assert!(config.theme.is_none() && config.problems.is_empty());
    }
}
