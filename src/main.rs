mod app;
mod codex_name;
mod config;
mod hook;
mod model;
mod paths;
mod provider;
mod store;
mod title;
mod tmux;

use std::process::ExitCode;

fn main() -> ExitCode {
    match app::run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("wips: {error:#}");
            ExitCode::FAILURE
        }
    }
}
