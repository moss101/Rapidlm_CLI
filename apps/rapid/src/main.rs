//! RapidLM composition root. Domain logic lives in workspace crates.

#![forbid(unsafe_code)]

fn main() {
    match rapid::run() {
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
