//! Themes: the colours and weights the screen is painted with.
//!
//! The painter produces plain rows and, beside them, a list of [`Span`]s saying
//! which [`Role`] each stretch of a row plays (status bar, composer, the user's
//! own lines, errors, ...). A [`Theme`] maps roles to [`Style`]s, and [`paint`]
//! turns rows plus spans into text with the escape sequences in it. Nothing
//! else in the TUI knows about colour, so a theme can neither move text nor
//! change what is shown, only how it looks.
//!
//! A style is written the way a person would say it: `bold`, `red`,
//! `black on cyan`, `bold underline #ff8800`, `dim bright-black`, `on 238`.
//! Words are attributes (`bold dim italic underline reverse`), colours (the
//! sixteen ANSI names with an optional `bright-`, a number 0-255, or
//! `#rrggbb`), and the word `on` before the background colour.
//!
//! Colour is off when [`crate::color::ColorMode`] is `Disabled` (`NO_COLOR`, a
//! dumb terminal): rows then come back exactly as painted. Truecolor is used
//! when `COLORTERM` says the terminal has it; otherwise an `#rrggbb` colour is
//! the nearest of the 256.

use std::collections::BTreeMap;

use crate::color::ColorMode;

/// What a stretch of the screen is.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Role {
    /// The status bar.
    Status,
    /// The prompt being typed.
    Composer,
    /// A line the user sent.
    User,
    /// The model's words.
    Assistant,
    /// Tool activity.
    Tool,
    /// Evidence and verification output.
    Evidence,
    /// A failure.
    Error,
    /// Notices and command output.
    System,
    /// An approval or protocol-error modal.
    Modal,
    /// A side panel.
    Sidebar,
}

impl Role {
    /// Every role, in the order a listing shows them.
    pub const ALL: [Self; 10] = [
        Self::Status,
        Self::Composer,
        Self::User,
        Self::Assistant,
        Self::Tool,
        Self::Evidence,
        Self::Error,
        Self::System,
        Self::Modal,
        Self::Sidebar,
    ];

    /// The name used in configuration.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Composer => "composer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Evidence => "evidence",
            Self::Error => "error",
            Self::System => "system",
            Self::Modal => "modal",
            Self::Sidebar => "sidebar",
        }
    }

    /// The role called `name` in configuration.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == name)
    }
}

/// A stretch of one screen row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    /// Row, from the top.
    pub row: u16,
    /// First column, inclusive.
    pub start: u16,
    /// Last column, exclusive.
    pub end: u16,
    /// What it is.
    pub role: Role,
}

/// A colour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    /// One of the 256 terminal colours (0-15 are the ANSI sixteen).
    Indexed(u8),
    /// A 24-bit colour.
    Rgb(u8, u8, u8),
}

/// How a role looks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

fn parse_color(word: &str) -> Option<Color> {
    if let Some(hex) = word.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
    }
    if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) {
        return word.parse::<u8>().ok().map(Color::Indexed);
    }
    let (base, bright) = match word.strip_prefix("bright-") {
        Some(rest) => (rest, true),
        None => (word, false),
    };
    let index = ANSI_NAMES.iter().position(|name| *name == base)?;
    Some(Color::Indexed(
        u8::try_from(index).ok()? + if bright { 8 } else { 0 },
    ))
}

impl Style {
    /// Parse a style as written in configuration.
    ///
    /// # Errors
    /// The word that is not an attribute or a colour, or a second foreground.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut style = Self::default();
        let mut background_next = false;
        for word in spec.split_whitespace() {
            let word = word.to_ascii_lowercase();
            if background_next {
                // After `on` only a colour will do.
                let color = parse_color(&word)
                    .ok_or_else(|| format!("`on` must be followed by a colour, not `{word}`"))?;
                if style.bg.is_some() {
                    return Err(format!("a second background colour: `{word}`"));
                }
                style.bg = Some(color);
                background_next = false;
                continue;
            }
            match word.as_str() {
                "on" => background_next = true,
                "bold" => style.bold = true,
                "dim" => style.dim = true,
                "italic" => style.italic = true,
                "underline" => style.underline = true,
                "reverse" => style.reverse = true,
                // `plain` is only a word to write when nothing else is wanted.
                "plain" | "none" => {
                    if style != Self::default() {
                        return Err(format!("`{word}` cannot follow other style words"));
                    }
                }
                other => {
                    let color = parse_color(other)
                        .ok_or_else(|| format!("`{other}` is not an attribute or a colour"))?;
                    if style.fg.is_some() {
                        return Err(format!("a second foreground colour: `{other}`"));
                    }
                    style.fg = Some(color);
                }
            }
        }
        if background_next {
            return Err("`on` must be followed by a colour".to_owned());
        }
        Ok(style)
    }

    /// Whether the style changes nothing.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    /// The escape sequence that turns the style on, for a terminal with the
    /// given colour depth. Empty for a plain style.
    #[must_use]
    pub fn open(&self, truecolor: bool) -> String {
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".to_owned());
        }
        if self.dim {
            codes.push("2".to_owned());
        }
        if self.italic {
            codes.push("3".to_owned());
        }
        if self.underline {
            codes.push("4".to_owned());
        }
        if self.reverse {
            codes.push("7".to_owned());
        }
        if let Some(color) = self.fg {
            codes.push(color_code(color, false, truecolor));
        }
        if let Some(color) = self.bg {
            codes.push(color_code(color, true, truecolor));
        }
        if codes.is_empty() {
            String::new()
        } else {
            format!("\u{1b}[{}m", codes.join(";"))
        }
    }
}

fn color_code(color: Color, background: bool, truecolor: bool) -> String {
    let base = if background { 40 } else { 30 };
    match color {
        Color::Indexed(n) if n < 8 => format!("{}", base + u16::from(n)),
        Color::Indexed(n) if n < 16 => format!("{}", base + 60 + u16::from(n - 8)),
        Color::Indexed(n) => format!("{};5;{n}", base + 8),
        Color::Rgb(r, g, b) if truecolor => format!("{};2;{r};{g};{b}", base + 8),
        Color::Rgb(r, g, b) => format!("{};5;{}", base + 8, nearest_256(r, g, b)),
    }
}

/// The 256-colour index closest to an RGB colour: the better of the 6x6x6 cube
/// and the 24-step grey ramp.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| -> u8 {
        // Cube steps are 0, 95, 135, 175, 215, 255.
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0;
        for (i, step) in STEPS.iter().enumerate() {
            if v.abs_diff(*step) < v.abs_diff(STEPS[best]) {
                best = i;
            }
        }
        u8::try_from(best).unwrap_or(0)
    };
    let cube = |l: u8| -> u8 { [0u8, 95, 135, 175, 215, 255][usize::from(l)] };
    let (lr, lg, lb) = (level(r), level(g), level(b));
    let cube_index = 16 + 36 * lr + 6 * lg + lb;
    let distance = |c: (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    let cube_distance = distance((cube(lr), cube(lg), cube(lb)));
    let average = u8::try_from((u16::from(r) + u16::from(g) + u16::from(b)) / 3).unwrap_or(0);
    let grey_step =
        u8::try_from(((u16::from(average.saturating_sub(8)) + 5) / 10).min(23)).unwrap_or(0);
    let grey_value = 8 + 10 * grey_step;
    let grey_distance = distance((grey_value, grey_value, grey_value));
    if grey_distance < cube_distance {
        232 + grey_step
    } else {
        cube_index
    }
}

/// Roles to styles. A role with no entry is painted as it came.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Theme {
    name: String,
    styles: BTreeMap<Role, Style>,
}

impl Theme {
    /// No styling at all: the screen exactly as painted.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            name: "default".to_owned(),
            styles: BTreeMap::new(),
        }
    }

    /// The built-in theme called `name`: `default`, `dark`, `light` or `mono`.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        let spec = |pairs: &[(Role, &str)]| -> BTreeMap<Role, Style> {
            pairs
                .iter()
                .filter_map(|(role, text)| Style::parse(text).ok().map(|s| (*role, s)))
                .collect()
        };
        let styles = match name {
            "default" => BTreeMap::new(),
            "dark" => spec(&[
                (Role::Status, "black on bright-black"),
                (Role::User, "bold cyan"),
                (Role::Tool, "dim"),
                (Role::Evidence, "green"),
                (Role::Error, "bold red"),
                (Role::System, "yellow"),
                (Role::Modal, "bold yellow"),
                (Role::Sidebar, "dim"),
            ]),
            "light" => spec(&[
                (Role::Status, "white on blue"),
                (Role::User, "bold blue"),
                (Role::Tool, "dim"),
                (Role::Evidence, "green"),
                (Role::Error, "bold red"),
                (Role::System, "magenta"),
                (Role::Modal, "bold magenta"),
                (Role::Sidebar, "dim"),
            ]),
            "mono" => spec(&[
                (Role::Status, "reverse"),
                (Role::User, "bold"),
                (Role::Tool, "dim"),
                (Role::Error, "bold underline"),
                (Role::Modal, "bold"),
                (Role::Sidebar, "dim"),
            ]),
            _ => return None,
        };
        Some(Self {
            name: name.to_owned(),
            styles,
        })
    }

    /// Names of the built-in themes.
    pub const BUILTIN: [&'static str; 4] = ["default", "dark", "light", "mono"];

    /// This theme's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The style of `role`, if the theme sets one.
    #[must_use]
    pub fn style(&self, role: Role) -> Option<&Style> {
        self.styles.get(&role)
    }

    /// Replace one role's style.
    pub fn set(&mut self, role: Role, style: Style) {
        if style.is_plain() {
            self.styles.remove(&role);
        } else {
            self.styles.insert(role, style);
        }
    }

    /// Whether the theme styles nothing.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.styles.is_empty()
    }
}

/// Whether the terminal advertises 24-bit colour, from an injected environment.
#[must_use]
pub fn truecolor_with(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("COLORTERM").is_some_and(|v| {
        let v = v.to_ascii_lowercase();
        v == "truecolor" || v == "24bit"
    })
}

/// The rows of a screen with the theme applied.
///
/// `rows` are the painted rows (one `char` per column, as the painter builds
/// them) and `spans` say what each stretch is; where spans overlap the later one
/// wins, which is how a modal covers the transcript beneath it. Every styled run
/// is closed with a reset before the row ends, so no style leaks into the next
/// row or onto the terminal after exit. With colour off, or a theme that styles
/// nothing, the rows come back untouched.
#[must_use]
pub fn paint(
    theme: &Theme,
    mode: ColorMode,
    truecolor: bool,
    rows: &[String],
    spans: &[Span],
) -> Vec<String> {
    if mode == ColorMode::Disabled || theme.is_plain() {
        return rows.to_vec();
    }
    rows.iter()
        .enumerate()
        .map(|(y, row)| {
            let Ok(y) = u16::try_from(y) else {
                return row.clone();
            };
            let chars: Vec<char> = row.chars().collect();
            let mut roles: Vec<Option<Role>> = vec![None; chars.len()];
            for span in spans.iter().filter(|s| s.row == y) {
                let end = usize::from(span.end).min(chars.len());
                for cell in roles.iter_mut().take(end).skip(usize::from(span.start)) {
                    *cell = Some(span.role);
                }
            }
            if roles
                .iter()
                .all(|r| r.is_none_or(|role| theme.style(role).is_none()))
            {
                return row.clone();
            }
            let mut out = String::with_capacity(row.len() + 16);
            let mut open: Option<Role> = None;
            for (i, ch) in chars.iter().enumerate() {
                let want = roles[i].filter(|role| theme.style(*role).is_some());
                if want != open {
                    if open.is_some() {
                        out.push_str("\u{1b}[0m");
                    }
                    if let Some(role) = want
                        && let Some(style) = theme.style(role)
                    {
                        out.push_str(&style.open(truecolor));
                    }
                    open = want;
                }
                out.push(*ch);
            }
            if open.is_some() {
                out.push_str("\u{1b}[0m");
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(row: u16, start: u16, end: u16, role: Role) -> Span {
        Span {
            row,
            start,
            end,
            role,
        }
    }

    #[test]
    fn styles_are_read_the_way_they_are_said() {
        assert_eq!(
            Style::parse("bold red").unwrap(),
            Style {
                bold: true,
                fg: Some(Color::Indexed(1)),
                ..Style::default()
            }
        );
        assert_eq!(
            Style::parse("black on bright-cyan").unwrap(),
            Style {
                fg: Some(Color::Indexed(0)),
                bg: Some(Color::Indexed(14)),
                ..Style::default()
            }
        );
        assert_eq!(
            Style::parse("UNDERLINE #FF8800 on 238").unwrap(),
            Style {
                underline: true,
                fg: Some(Color::Rgb(255, 136, 0)),
                bg: Some(Color::Indexed(238)),
                ..Style::default()
            }
        );
        assert!(Style::parse("").unwrap().is_plain());
        assert!(Style::parse("plain").unwrap().is_plain());
    }

    #[test]
    fn nonsense_is_named_and_refused() {
        assert!(Style::parse("loud").unwrap_err().contains("`loud`"));
        assert!(
            Style::parse("red blue")
                .unwrap_err()
                .contains("second foreground")
        );
        assert!(
            Style::parse("red on")
                .unwrap_err()
                .contains("followed by a colour")
        );
        assert!(Style::parse("on on red").is_err());
        assert!(
            Style::parse("on bold").is_err(),
            "an attribute is not a background"
        );
        assert!(Style::parse("#12345").is_err());
        assert!(Style::parse("#gg0000").is_err());
        assert!(Style::parse("999").is_err(), "256 is past the palette");
        assert!(Style::parse("on red on bold").is_err(), "a second `on`");
        assert!(Style::parse("on plain").is_err());
        assert!(
            Style::parse("bold plain").is_err(),
            "plain after other words"
        );
        assert!(
            Style::parse("on red on blue").is_err(),
            "a second background"
        );
        assert!(Style::parse("+5").is_err() && Style::parse("-1").is_err());
    }

    #[test]
    fn escape_sequences_match_the_colour_depth() {
        let style = Style::parse("bold dim italic underline reverse red on bright-blue").unwrap();
        assert_eq!(style.open(false), "\u{1b}[1;2;3;4;7;31;104m");
        assert_eq!(Style::parse("208").unwrap().open(false), "\u{1b}[38;5;208m");
        assert_eq!(
            Style::parse("on 238").unwrap().open(false),
            "\u{1b}[48;5;238m"
        );
        assert_eq!(
            Style::parse("#ff8800").unwrap().open(true),
            "\u{1b}[38;2;255;136;0m"
        );
        // Without truecolor, an RGB colour is the nearest of the 256.
        assert_eq!(
            Style::parse("#ff0000").unwrap().open(false),
            "\u{1b}[38;5;196m"
        );
        assert_eq!(
            Style::parse("#808080").unwrap().open(false),
            "\u{1b}[38;5;244m"
        );
        assert_eq!(Style::default().open(true), "");
    }

    #[test]
    fn nearest_256_prefers_the_grey_ramp_for_greys_and_the_cube_for_colours() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert!((232..=255).contains(&nearest_256(100, 100, 100)));
        // Grey steps are rounded, not floored: 17 is nearer 233 (18) than 232 (8).
        assert_eq!(nearest_256(17, 17, 17), 233);
        assert_eq!(nearest_256(0, 255, 0), 46);
    }

    #[test]
    fn truecolor_is_read_from_colorterm() {
        let env = |v: &'static str| move |k: &str| (k == "COLORTERM").then(|| v.to_owned());
        assert!(truecolor_with(&env("truecolor")));
        assert!(truecolor_with(&env("24BIT")));
        assert!(!truecolor_with(&env("")));
        assert!(!truecolor_with(&|_| None));
    }

    #[test]
    fn built_in_themes_exist_and_default_styles_nothing() {
        for name in Theme::BUILTIN {
            assert!(Theme::builtin(name).is_some(), "{name}");
        }
        assert!(Theme::builtin("default").unwrap().is_plain());
        assert!(Theme::builtin("neon").is_none());
        let dark = Theme::builtin("dark").unwrap();
        assert!(dark.style(Role::Error).unwrap().bold);
        assert_eq!(Role::from_name("error"), Some(Role::Error));
        assert_eq!(Role::from_name("nope"), None);
        for role in Role::ALL {
            assert_eq!(Role::from_name(role.name()), Some(role));
        }
    }

    #[test]
    fn a_painted_row_is_wrapped_in_its_style_and_reset_before_the_row_ends() {
        let mut theme = Theme::plain();
        theme.set(Role::Error, Style::parse("bold red").unwrap());
        let rows = vec!["ok  boom  ".to_owned(), "next".to_owned()];
        let painted = paint(
            &theme,
            ColorMode::Enabled,
            false,
            &rows,
            &[span(0, 4, 8, Role::Error)],
        );
        assert_eq!(painted[0], "ok  \u{1b}[1;31mboom\u{1b}[0m  ");
        assert_eq!(painted[1], "next", "a row with no span is untouched");
    }

    #[test]
    fn a_later_span_covers_an_earlier_one_and_roles_without_a_style_are_skipped() {
        let mut theme = Theme::plain();
        theme.set(Role::Modal, Style::parse("reverse").unwrap());
        let rows = vec!["abcdefgh".to_owned()];
        let spans = [span(0, 0, 8, Role::Assistant), span(0, 2, 5, Role::Modal)];
        let painted = paint(&theme, ColorMode::Enabled, false, &rows, &spans);
        assert_eq!(painted[0], "ab\u{1b}[7mcde\u{1b}[0mfgh");
    }

    #[test]
    fn adjacent_roles_close_one_style_before_opening_the_next() {
        let mut theme = Theme::plain();
        theme.set(Role::User, Style::parse("bold").unwrap());
        theme.set(Role::Tool, Style::parse("dim").unwrap());
        let rows = vec!["aabb".to_owned()];
        let spans = [span(0, 0, 2, Role::User), span(0, 2, 4, Role::Tool)];
        let painted = paint(&theme, ColorMode::Enabled, false, &rows, &spans);
        assert_eq!(painted[0], "\u{1b}[1maa\u{1b}[0m\u{1b}[2mbb\u{1b}[0m");
    }

    #[test]
    fn no_colour_or_no_theme_returns_the_rows_exactly() {
        let mut theme = Theme::plain();
        theme.set(Role::Error, Style::parse("red").unwrap());
        let rows = vec!["boom".to_owned()];
        let spans = [span(0, 0, 4, Role::Error)];
        assert_eq!(
            paint(&theme, ColorMode::Disabled, true, &rows, &spans),
            rows
        );
        assert_eq!(
            paint(&Theme::plain(), ColorMode::Enabled, true, &rows, &spans),
            rows
        );
    }

    #[test]
    fn spans_past_the_row_are_clipped_and_multibyte_rows_are_cut_by_column() {
        let mut theme = Theme::plain();
        theme.set(Role::Status, Style::parse("reverse").unwrap());
        let rows = vec!["héllo→".to_owned()];
        let painted = paint(
            &theme,
            ColorMode::Enabled,
            false,
            &rows,
            &[span(0, 1, 99, Role::Status)],
        );
        assert_eq!(painted[0], "h\u{1b}[7méllo→\u{1b}[0m");
    }

    #[test]
    fn setting_a_plain_style_removes_the_role() {
        let mut theme = Theme::builtin("dark").unwrap();
        assert!(theme.style(Role::Error).is_some());
        theme.set(Role::Error, Style::default());
        assert!(theme.style(Role::Error).is_none());
    }
}
