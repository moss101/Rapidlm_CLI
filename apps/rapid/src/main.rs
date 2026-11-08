//! RapidLM composition root. Domain logic lives in workspace crates.

#![forbid(unsafe_code)]

fn main() {
    let result = rapid::run();
    // What failed is said first: the observer hooks' queue is waited on after
    // it, never in front of it.
    let exit = match result {
        Ok(code) => code,
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
            err.exit_code()
        }
    };
    // Observer-stage hooks run on a worker thread; give what is still queued
    // a bounded chance to run before the process goes, and say so when it was
    // not enough.
    if !rapid::observer_hooks::drain(std::time::Duration::from_secs(10)) {
        let warning = "observer hooks were still running after 10 seconds; the events not yet \
run were dropped";
        // The same shape as the error before it, in JSON mode.
        if rapid::agent_mode::current().output == Some(rapid::agent_mode::Output::Json) {
            eprintln!("{}", serde_json::json!({ "warning": warning }));
        } else {
            eprintln!("warning: {warning}");
        }
    }
    if exit != 0 {
        std::process::exit(exit);
    }
}
