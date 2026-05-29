mod agent_tools;
mod backend;
mod cli;
mod config;
mod context;
mod gateway;
mod id;
mod memory;
mod orchestrator;
mod policy;
mod prompt;
mod responses_agent;
mod sandbox;
mod session;
mod tui;

use std::process;

fn main() {
    let cli = cli::parse_args(std::env::args().skip(1).collect());
    let code = match cli::run(cli) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    };

    process::exit(code);
}
