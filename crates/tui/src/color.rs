//! Color-mode resolution honoring `NO_COLOR` / dumb terminals (P10-006).
//!
//! Pure and testable: environment lookup is injected so tests drive every
//! combination without mutating process state.

/// Effective color policy for TUI rendering layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    Enabled,
    Disabled,
}

fn lookup(env: &(dyn Fn(&str) -> Option<String> + '_), key: &str) -> Option<String> {
    env(key)
}

/// Resolve the mode from an injected environment map (test seam).
pub fn resolve_color_mode_with(env: &dyn Fn(&str) -> Option<String>) -> ColorMode {
    // NO_COLOR: any non-empty value disables color (https://no-color.org).
    match lookup(env, "NO_COLOR") {
        Some(value) if !value.trim().is_empty() => return ColorMode::Disabled,
        _ => {}
    }
    // Dumb terminals cannot repaint reliably.
    if matches!(lookup(env, "TERM").as_deref(), Some("dumb")) {
        return ColorMode::Disabled;
    }
    ColorMode::Enabled
}

/// Resolve from the real process environment.
pub fn resolve_color_mode() -> ColorMode {
    resolve_color_mode_with(&|key| std::env::var(key).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn no_color_any_non_empty_value_disables_color() {
        assert_eq!(
            resolve_color_mode_with(&env_of(&[("NO_COLOR", "1")])),
            ColorMode::Disabled
        );
        assert_eq!(
            resolve_color_mode_with(&env_of(&[("NO_COLOR", "")])),
            ColorMode::Enabled,
            "empty NO_COLOR does not disable"
        );
    }

    #[test]
    fn dumb_term_disable_and_default_enable() {
        assert_eq!(
            resolve_color_mode_with(&env_of(&[("TERM", "dumb")])),
            ColorMode::Disabled
        );
        assert_eq!(
            resolve_color_mode_with(&env_of(&[("TERM", "xterm-256color")])),
            ColorMode::Enabled
        );
        assert_eq!(resolve_color_mode_with(&env_of(&[])), ColorMode::Enabled);
        // NO_COLOR wins over TERM when both present.
        assert_eq!(
            resolve_color_mode_with(&env_of(&[("NO_COLOR", "1"), ("TERM", "xterm")])),
            ColorMode::Disabled
        );
    }
}
