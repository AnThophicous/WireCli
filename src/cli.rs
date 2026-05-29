use crate::agent_tools::BoxTools;
use crate::backend::select_backend;
use crate::config::{AppConfig, AppPaths};
use crate::mcp::McpRegistry;
use crate::memory::AnchorStore;
use crate::prompt::base_developer_prompt;
use crate::responses_agent;
use crate::sandbox::SandboxManager;
use crate::session::{SessionEvent, SessionStore};

#[derive(Debug, Clone)]
pub enum Command {
    Tui,
    Sessions,
    Providers,
    Models,
    Mcp,
    Run { prompt: String },
    Lattice { action: LatticeAction },
    Help,
    Version,
}

#[derive(Debug, Clone)]
pub enum LatticeAction {
    New {
        name: String,
    },
    List,
    Run {
        cell_id: String,
        command: Vec<String>,
    },
    Tools,
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Command,
}

pub fn parse_args(args: Vec<String>) -> Cli {
    if args.first().map(|s| s.as_str()) == Some("riftcli") {
        return parse_args(args.into_iter().skip(1).collect());
    }

    let command = match args.first().map(|s| s.as_str()) {
        None => Command::Tui,
        Some("-h") | Some("--help") => Command::Help,
        Some("-V") | Some("--version") => Command::Version,
        Some("tui") => Command::Tui,
        Some("sessions") => Command::Sessions,
        Some("providers") => Command::Providers,
        Some("models") => Command::Models,
        Some("mcp") => Command::Mcp,
        Some("box") => parse_lattice_command(&args[1..]),
        Some("run") => {
            let prompt = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            Command::Run { prompt }
        }
        _ => Command::Help,
    };

    Cli { command }
}

fn parse_lattice_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        Some("new") | Some("init") => {
            let name = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            Command::Lattice {
                action: LatticeAction::New { name },
            }
        }
        Some("list") | None => Command::Lattice {
            action: LatticeAction::List,
        },
        Some("run") | Some("exec") => {
            let cell_id = args.get(1).cloned().unwrap_or_default();
            let command = parse_command_tail(&args[2..]);
            Command::Lattice {
                action: LatticeAction::Run { cell_id, command },
            }
        }
        Some("tools") | Some("toolbox") => Command::Lattice {
            action: LatticeAction::Tools,
        },
        _ => Command::Help,
    }
}

fn parse_command_tail(args: &[String]) -> Vec<String> {
    if let Some(pos) = args.iter().position(|arg| arg == "--") {
        return args.iter().skip(pos + 1).cloned().collect();
    }
    args.to_vec()
}

pub fn run(cli: Cli) -> Result<(), String> {
    let paths = AppPaths::detect()?;

    match cli.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("riftcli 0.1.0");
            Ok(())
        }
        Command::Tui => crate::tui::run_tui(paths),
        Command::Sessions => crate::tui::run_sessions_tui(paths),
        Command::Providers => list_providers(&paths),
        Command::Models => list_models(&paths),
        Command::Mcp => list_mcp(&paths),
        Command::Run { prompt } => run_prompt(&paths, prompt),
        Command::Lattice { action } => lattice_command(&paths, action),
    }
}

fn list_providers(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    println!("local  {}", config.base_url);
    Ok(())
}

fn list_models(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let models: serde_json::Value = runtime.block_on(async {
        let url = format!("{}/models", config.base_url.trim_end_matches('/'));
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("models endpoint returned {}: {}", status, value));
        }
        Ok(value)
    })?;

    if let Some(data) = models.get("data").and_then(|v| v.as_array()) {
        for model in data {
            let id = model.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let owned_by = model
                .get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            println!("{id}  {owned_by}");
        }
    } else {
        println!("{}", models);
    }
    Ok(())
}

fn list_mcp(paths: &AppPaths) -> Result<(), String> {
    let registry = McpRegistry::load(paths)?;
    if registry.servers().is_empty() {
        println!("no mcp servers configured");
        return Ok(());
    }

    println!("configured servers:");
    for server in registry.servers() {
        println!("- {}  {}", server.name, server.command);
    }

    let tools = registry.discover_tools()?;
    if tools.is_empty() {
        println!("no mcp tools discovered");
    } else {
        println!();
        println!("discovered tools:");
        for tool in tools {
            println!(
                "- {}  ({}::{})",
                tool.function_name, tool.server_name, tool.tool_name
            );
        }
    }

    Ok(())
}

fn run_prompt(paths: &AppPaths, prompt: String) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("missing prompt".to_string());
    }

    let config = AppConfig::load(paths)?;
    if matches!(config.provider.as_str(), "local" | "responses" | "codex") {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let (session_id, _output) =
            runtime.block_on(responses_agent::run_prompt(paths, &config, prompt.clone()))?;
        println!("session: {}", session_id);
        return Ok(());
    }

    let backend = select_backend(&config);
    let mut store = SessionStore::new(paths)?;
    let session = store.create(
        &paths.project_key,
        &paths.root_dir.display().to_string(),
        &prompt,
    )?;
    store.append_event(
        &paths.project_key,
        &session.id,
        SessionEvent::developer(base_developer_prompt()),
    )?;
    store.append_event(
        &paths.project_key,
        &session.id,
        SessionEvent::user(prompt.clone()),
    )?;

    let output = backend.respond(&prompt);
    store.append_command(
        &paths.project_key,
        &session.id,
        &vec![
            "model.invoke".to_string(),
            config.provider.clone(),
            config.model.clone(),
        ],
        "ok",
        Some(0),
        &output.text,
        "",
    )?;
    store.append_event(
        &paths.project_key,
        &session.id,
        SessionEvent::assistant(output.text.clone()),
    )?;

    println!("{}", output.text);
    println!("session: {}", session.id);
    Ok(())
}

fn lattice_command(paths: &AppPaths, action: LatticeAction) -> Result<(), String> {
    let manager = SandboxManager::new(paths)?;
    let anchors = AnchorStore::new(paths)?;
    match action {
        LatticeAction::New { name } => {
            let summary = manager.create(&name)?;
            println!("box created");
            println!("id: {}", summary.id);
            println!("name: {}", summary.name);
            println!(
                "workspace: {}/{}",
                paths.sandboxes_dir.display(),
                summary.id
            );
        }
        LatticeAction::List => {
            let cells = manager.list()?;
            if cells.is_empty() {
                println!("no boxes");
            } else {
                for cell in cells {
                    println!(
                        "{}  {}  {}  {}",
                        cell.id, cell.state, cell.created_at, cell.name
                    );
                }
            }
        }
        LatticeAction::Run { cell_id, command } => {
            if cell_id.trim().is_empty() {
                return Err("missing cell id".to_string());
            }
            let status = manager.run(&cell_id, &command)?;
            if !status.success() {
                return Err(match status.code() {
                    Some(code) => format!("cell command exited with code {code}"),
                    None => "cell command terminated by signal".to_string(),
                });
            }
        }
        LatticeAction::Tools => {
            let tools = BoxTools::new(&manager, &anchors);
            for tool in tools.list() {
                println!("{tool}");
            }
        }
    }
    Ok(())
}

fn print_help() {
    println!("riftcli 0.1.0");
    println!();
    println!("usage:");
    println!("  riftcli");
    println!("  riftcli tui");
    println!("  riftcli sessions");
    println!("  riftcli run <prompt...>");
    println!("  riftcli models");
    println!("  riftcli providers");
    println!("  riftcli mcp");
    println!("  riftcli box [new|list|run|tools]");
    println!("  riftcli --help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_runs_prompt_command() {
        let cli = parse_args(vec![
            "run".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ]);
        match cli.command {
            Command::Run { prompt } => assert_eq!(prompt, "hello world"),
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn parse_box_alias() {
        let cli = parse_args(vec!["box".to_string(), "list".to_string()]);
        match cli.command {
            Command::Lattice { action } => match action {
                LatticeAction::List => {}
                _ => panic!("expected list action"),
            },
            _ => panic!("expected lattice command"),
        }
    }

    #[test]
    fn parse_box_tools() {
        let cli = parse_args(vec!["box".to_string(), "tools".to_string()]);
        match cli.command {
            Command::Lattice { action } => match action {
                LatticeAction::Tools => {}
                _ => panic!("expected tools action"),
            },
            _ => panic!("expected lattice command"),
        }
    }

    #[test]
    fn parse_binary_name_aliases_to_tui() {
        let cli = parse_args(vec!["riftcli".to_string()]);
        match cli.command {
            Command::Tui => {}
            _ => panic!("expected tui command"),
        }
    }

    #[test]
    fn parse_mcp_command() {
        let cli = parse_args(vec!["mcp".to_string()]);
        match cli.command {
            Command::Mcp => {}
            _ => panic!("expected mcp command"),
        }
    }

    #[test]
    fn parse_sessions_command() {
        let cli = parse_args(vec!["sessions".to_string()]);
        match cli.command {
            Command::Sessions => {}
            _ => panic!("expected sessions command"),
        }
    }
}
