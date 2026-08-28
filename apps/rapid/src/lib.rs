//! RapidLM CLI library surface. The binary is a thin composition root.

#![forbid(unsafe_code)]

pub mod computer_runtime;
pub mod external_agents;
pub mod goal_claim;
pub mod goal_host;
pub mod headless;
pub mod host;
pub mod host_runtime;
pub mod interactive;
pub mod managed_config;
pub mod model;
pub mod p9_commands;
pub mod preview;
pub mod user_config;

pub use interactive::{
    CLI_USAGE, InteractiveError, InteractiveInput, InteractiveOptions, InteractiveOutcome,
    InteractiveReport, LaunchMode, classify_launch, run, run_interactive,
};
