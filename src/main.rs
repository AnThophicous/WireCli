mod backend;
mod cli;
mod config;
mod id;
mod orchestrator;
mod policy;
mod sandbox;
mod session;

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
