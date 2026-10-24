//! RapidLM composition root. Domain logic lives in workspace crates.

#![forbid(unsafe_code)]

fn main() {
    let result = rapid::run();
    // Observer-stage hooks run on a worker thread; give what is still queued
    // a bounded chance to run before the process goes.
    rapid::observer_hooks::drain(std::time::Duration::from_secs(10));
    match result {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // JSON mode writes the failure as the `{"error": {…}}` envelope;
            // otherwise the message, then the next command to try. The exit
            // code is the same either way.
            if rapid::agent_mode::current().output == Some(rapid::agent_mode::Output::Json) {
                eprintln!("{}", err.to_cli_error().to_json_line());
            } else {
                eprintln!("{err}");
                eprintln!("hint: {}", err.hint());
            }
            std::process::exit(err.exit_code());
        }
    }
}
