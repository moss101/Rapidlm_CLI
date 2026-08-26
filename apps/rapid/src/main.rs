//! RapidLM composition root. Domain logic lives in workspace crates.

#![forbid(unsafe_code)]

fn main() {
    match rapid::run() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(err.exit_code());
        }
    }
}
