#![allow(dead_code)]

mod acp;
mod agent_tools;
mod approvals;
mod backend;
mod cli;
mod commands;
mod config;
mod context;
mod flash_cache;
mod gateway;
mod guardian;
mod harness;
mod hooks;
mod id;
mod lab;
mod mcp;
mod memory;
mod memory_context;
mod model_catalog;
mod models;
mod orchestrator;
mod policy;
mod prompt;
mod providers;
mod responses_agent;
mod safekey;
mod sandbox;
mod session;
mod skills;
mod startup;
mod subagents;
mod tui;
mod verifier;
mod wire_auth;
mod wire_memory;

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
