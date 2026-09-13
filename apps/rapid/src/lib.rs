//! RapidLM CLI library surface. The binary is a thin composition root.

#![forbid(unsafe_code)]

pub mod approvals;
pub mod command_help;
pub mod computer_runtime;
pub mod context_retrieval;
pub mod doctor;
pub mod exec_diag;
pub mod exec_tools;
pub mod external_agents;
pub mod external_scan;
pub mod findings_store;
pub mod goal_claim;
pub mod goal_host;
pub mod headless;
pub mod hooks;
pub mod host;
pub mod host_runtime;
pub mod interactive;
pub mod line_diff;
pub mod managed_config;
pub mod mcp_admin;
pub mod mcp_config;
pub mod model;
pub mod p9_commands;
pub mod pdf_text;
pub mod permissions;
pub mod permissions_cli;
pub mod preview;
pub mod sandbox_exec;
pub mod shadow_diagnostics;
pub mod structured_output;
pub mod user_config;
pub mod web_fetch;

pub use interactive::{
    CLI_USAGE, InteractiveError, InteractiveInput, InteractiveOptions, InteractiveOutcome,
    InteractiveReport, LaunchMode, classify_launch, run, run_interactive,
};
