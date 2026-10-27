//! Named, reusable prompt chains: `/presets`.
//!
//! A preset is a name plus an ordered list of steps, each step a prompt or
//! a slash command, saved in the user's home (`presets.json`) so it
//! survives sessions and is the user's own — never a project's (a project
//! that wants a reusable workflow ships a playbook, the heavyweight
//! answer; a preset is the lightweight one).
//!
//! Running a preset replays its steps in order through the same paths
//! typed input takes: a slash step dispatches as a slash command, a text
//! step submits as a turn — and a turn submitted while one runs queues
//! behind it, exactly as typed input does, so a chain runs sequentially
//! without a second executor.

use std::path::Path;

/// Most presets one home may hold.
pub const MAX_PRESETS: usize = 32;
/// Most steps one preset may hold.
pub const MAX_STEPS: usize = 8;
/// Longest one step may be — a prompt, not a document.
pub const MAX_STEP_BYTES: usize = 4096;
/// Longest a preset name may be.
pub const MAX_NAME_BYTES: usize = 32;
/// Bound on the store document as a whole.
pub const MAX_STORE_BYTES: usize = 128 * 1024;

/// One named chain of steps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preset {
    pub name: String,
    pub steps: Vec<String>,
}

/// The user's presets, loaded from and saved to `presets.json` beside the
/// user config. A missing file is an empty store; one that cannot be
/// parsed is too (fail open — a broken store must not break the session),
/// and `save` refuses to write a store its own loader would reject.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Presets {
    pub presets: Vec<Preset>,
}

/// Typed store failure. Never echoes document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresetError {
    TooLarge,
    InvalidJson,
    TooManyPresets,
    TooManySteps,
    BadName,
    BadStep,
    Unknown,
    Empty,
}

impl PresetError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "the preset store exceeds its size bound",
            Self::InvalidJson => "the preset store is not valid JSON",
            Self::TooManyPresets => "too many presets",
            Self::TooManySteps => "a preset may hold at most 8 steps",
            Self::BadName => "preset names are lowercase letters, digits and dashes",
            Self::BadStep => "a step is empty or too long (4096 bytes)",
            Self::Unknown => "no preset by that name",
            Self::Empty => "a preset needs at least one step",
        }
    }
}

impl Presets {
    /// Load from `<home>/presets.json`; missing or broken reads as empty.
    pub fn load(home: &Path) -> Self {
        Self::parse(&std::fs::read_to_string(home.join("presets.json")).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Parse a store document (pure).
    pub fn parse(text: &str) -> Result<Self, PresetError> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        if text.len() > MAX_STORE_BYTES {
            return Err(PresetError::TooLarge);
        }
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| PresetError::InvalidJson)?;
        let entries = value
            .get("presets")
            .and_then(serde_json::Value::as_array)
            .ok_or(PresetError::InvalidJson)?;
        let mut presets = Vec::new();
        for entry in entries {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or(PresetError::InvalidJson)?;
            let steps = entry
                .get("steps")
                .and_then(serde_json::Value::as_array)
                .ok_or(PresetError::InvalidJson)?;
            let steps: Vec<String> = steps
                .iter()
                .map(|step| {
                    step.as_str()
                        .filter(|text| !text.trim().is_empty() && text.len() <= MAX_STEP_BYTES)
                        .map(str::to_owned)
                        .ok_or(PresetError::BadStep)
                })
                .collect::<Result<_, _>>()?;
            let preset = Preset {
                name: name.to_owned(),
                steps,
            };
            validate(&preset)?;
            presets.push(preset);
            if presets.len() > MAX_PRESETS {
                return Err(PresetError::TooManyPresets);
            }
        }
        Ok(Self { presets })
    }

    /// Save to `<home>/presets.json`.
    pub fn save(&self, home: &Path) -> Result<(), PresetError> {
        for preset in &self.presets {
            validate(preset)?;
        }
        let document = serde_json::json!({
            "presets": self
                .presets
                .iter()
                .map(|preset| serde_json::json!({
                    "name": preset.name,
                    "steps": preset.steps,
                }))
                .collect::<Vec<_>>(),
        });
        let body = serde_json::to_string(&document).map_err(|_| PresetError::InvalidJson)?;
        if body.len() > MAX_STORE_BYTES {
            return Err(PresetError::TooLarge);
        }
        std::fs::write(home.join("presets.json"), body).map_err(|_| PresetError::TooLarge)
    }

    /// The numbered listing `/presets` shows, one line per preset.
    pub fn listing(&self) -> Vec<String> {
        self.presets
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                let first = preset.steps.first().map(String::as_str).unwrap_or("");
                let shown: String = first.chars().take(60).collect();
                format!(
                    "{}. {} — {} step(s): {}",
                    index + 1,
                    preset.name,
                    preset.steps.len(),
                    shown
                )
            })
            .collect()
    }

    /// Add `step` to `name`, creating the preset when it does not exist —
    /// chains build one step at a time, in order.
    pub fn add_step(&mut self, name: &str, step: &str) -> Result<(), PresetError> {
        let step = step.trim();
        if !crate::permissions::valid_profile_name(name) || name.len() > MAX_NAME_BYTES {
            return Err(PresetError::BadName);
        }
        if step.is_empty() || step.len() > MAX_STEP_BYTES {
            return Err(PresetError::BadStep);
        }
        match self.presets.iter_mut().find(|preset| preset.name == name) {
            Some(preset) => {
                if preset.steps.len() >= MAX_STEPS {
                    return Err(PresetError::TooManySteps);
                }
                preset.steps.push(step.to_owned());
            }
            None => {
                if self.presets.len() >= MAX_PRESETS {
                    return Err(PresetError::TooManyPresets);
                }
                self.presets.push(Preset {
                    name: name.to_owned(),
                    steps: vec![step.to_owned()],
                });
            }
        }
        Ok(())
    }

    /// Remove `name`; `step` names one step (1-based) to drop instead of
    /// the whole preset, and a preset whose last step goes goes too.
    pub fn remove(&mut self, name: &str, step: Option<usize>) -> Result<(), PresetError> {
        let index = self
            .presets
            .iter()
            .position(|preset| preset.name == name)
            .ok_or(PresetError::Unknown)?;
        match step {
            None => {
                self.presets.remove(index);
            }
            Some(number) => {
                if number == 0 || number > self.presets[index].steps.len() {
                    return Err(PresetError::Unknown);
                }
                self.presets[index].steps.remove(number - 1);
                if self.presets[index].steps.is_empty() {
                    self.presets.remove(index);
                }
            }
        }
        Ok(())
    }

    /// The preset `name` holds, if any.
    pub fn get(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.name == name)
    }
}

fn validate(preset: &Preset) -> Result<(), PresetError> {
    if !crate::permissions::valid_profile_name(&preset.name) || preset.name.len() > MAX_NAME_BYTES {
        return Err(PresetError::BadName);
    }
    if preset.steps.is_empty() {
        return Err(PresetError::Empty);
    }
    if preset.steps.len() > MAX_STEPS {
        return Err(PresetError::TooManySteps);
    }
    if preset
        .steps
        .iter()
        .any(|step| step.trim().is_empty() || step.len() > MAX_STEP_BYTES)
    {
        return Err(PresetError::BadStep);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rapidlm-presets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn presets_round_trip_and_build_chains() {
        let home = home();
        let mut presets = Presets::load(&home);
        assert!(presets.presets.is_empty(), "a missing store is empty");
        presets.add_step("fmt", "/compact").expect("first step");
        presets
            .add_step("fmt", "run the formatter and fix what it flags")
            .expect("second step");
        presets
            .add_step("nightly", "summarise what changed today")
            .expect("another preset");
        presets.save(&home).expect("save");

        // The store round-trips: what was saved is what loads.
        let reloaded = Presets::load(&home);
        assert_eq!(reloaded, presets);
        let fmt = reloaded.get("fmt").expect("fmt");
        assert_eq!(fmt.steps.len(), 2);
        assert_eq!(fmt.steps[0], "/compact");

        // The listing is numbered and names the step count.
        assert!(
            reloaded
                .listing()
                .iter()
                .any(|line| line.contains("fmt") && line.contains("2 step(s)")),
            "{:?}",
            reloaded.listing()
        );

        // Removing one step keeps the rest; removing the last step takes
        // the preset with it.
        let mut edited = reloaded.clone();
        edited.remove("fmt", Some(1)).expect("drop step 1");
        assert_eq!(edited.get("fmt").expect("fmt").steps.len(), 1);
        edited.remove("fmt", Some(1)).expect("drop the last step");
        assert!(edited.get("fmt").is_none());

        edited.remove("nightly", None).expect("whole preset");
        assert!(edited.get("nightly").is_none());
        assert_eq!(
            edited.remove("nightly", None),
            Err(PresetError::Unknown),
            "removing what is not there is a typed failure"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn preset_bounds_and_names_are_enforced() {
        let mut presets = Presets::default();
        assert_eq!(
            presets.add_step("Not A Name", "x"),
            Err(PresetError::BadName)
        );
        assert_eq!(presets.add_step("ok", "   "), Err(PresetError::BadStep));
        assert_eq!(
            presets.add_step("ok", &"x".repeat(MAX_STEP_BYTES + 1)),
            Err(PresetError::BadStep)
        );
        for step in 0..MAX_STEPS {
            presets
                .add_step("chain", &format!("step {step}"))
                .expect("steps");
        }
        assert_eq!(
            presets.add_step("chain", "one too many"),
            Err(PresetError::TooManySteps)
        );
        // A broken document reads as empty, never as a half-parsed store.
        assert_eq!(Presets::parse("not json"), Err(PresetError::InvalidJson));
        assert_eq!(
            Presets::parse("{\"presets\": 3}"),
            Err(PresetError::InvalidJson)
        );
        assert_eq!(Presets::default(), Presets::parse("").expect("empty doc"));
    }
}
