use crate::agent_tools::BOX_TOOL_NAMES;
use crate::config::{AppConfig, AppPaths};
use crate::id::next_id;
use crate::lab::LabStore;
use crate::mcp::McpRegistry;
use crate::memory_context::MemoryContextStore;
use crate::policy::CommandPolicy;
use crate::providers::{active_provider, provider_model_mismatch, provider_protocol};
use crate::responses_agent::{
    self, AgentControl, AgentEvent, AgentObserver, PromptInput, TokenUsage,
};
use crate::safekey::{redact_secrets, write_private_file};
use crate::session::{SessionStore, TimelineEvent};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub enum HarnessAction {
    Help,
    Run(HarnessRunOptions),
    Replay { run_id: Option<String> },
    Inspect { run_id: Option<String> },
    Doctor,
    Evals,
}

#[derive(Debug, Clone)]
pub struct HarnessRunOptions {
    prompt_parts: Vec<String>,
    prompt_file: Option<PathBuf>,
    read_stdin: bool,
    session_id: Option<String>,
    output: HarnessOutput,
    verify: bool,
    policy: HarnessPolicy,
    label: Option<String>,
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessOutput {
    Ndjson,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessPolicy {
    Standard,
    Strict,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HarnessVerification {
    status: String,
    score: u8,
    findings: Vec<String>,
    tool_calls: usize,
    edited: bool,
    validation_seen: bool,
    duration_ms: u128,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HarnessMetadata {
    run_id: String,
    session_id: Option<String>,
    project_key: String,
    cwd: String,
    provider: String,
    protocol: String,
    model: String,
    label: Option<String>,
    started_at_ms: u128,
    duration_ms: u128,
    success: bool,
    dry_run: bool,
    prompt_preview: String,
    log_path: String,
    event_counts: BTreeMap<String, u64>,
    verification: Option<HarnessVerification>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ToolManifest {
    builtin_tools: Vec<String>,
    mcp_servers: usize,
    mcp_tools: Vec<String>,
    mcp_warnings: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HarnessPreflight {
    cwd: String,
    provider: String,
    protocol: String,
    model: String,
    permission_mode: String,
    policy: String,
    tools_builtin: usize,
    mcp_servers: usize,
    mcp_tools: usize,
    mcp_warnings: Vec<String>,
    memory_blocks: usize,
    git: Option<GitSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct GitSummary {
    branch: Option<String>,
    dirty_files: usize,
}

#[derive(Debug, Clone)]
struct ToolStart {
    name: String,
    started_at: Instant,
}

pub fn parse_action(args: &[String]) -> HarnessAction {
    match args.first().map(|value| value.as_str()) {
        None | Some("-h") | Some("--help") | Some("help") => HarnessAction::Help,
        Some("doctor") | Some("check") => HarnessAction::Doctor,
        Some("evals") | Some("benchmark") | Some("bench") => HarnessAction::Evals,
        Some("replay") => HarnessAction::Replay {
            run_id: args.get(1).filter(|value| !value.starts_with('-')).cloned(),
        },
        Some("inspect") | Some("show") => HarnessAction::Inspect {
            run_id: args.get(1).filter(|value| !value.starts_with('-')).cloned(),
        },
        Some("run") => HarnessAction::Run(parse_run_options(&args[1..])),
        _ => HarnessAction::Run(parse_run_options(args)),
    }
}

fn parse_run_options(args: &[String]) -> HarnessRunOptions {
    let mut options = HarnessRunOptions {
        prompt_parts: Vec::new(),
        prompt_file: None,
        read_stdin: false,
        session_id: None,
        output: HarnessOutput::Ndjson,
        verify: true,
        policy: HarnessPolicy::Standard,
        label: None,
        dry_run: false,
    };

    let mut free = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "-p" | "--prompt" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.prompt_parts.push(value.clone());
                }
            }
            "-f" | "--prompt-file" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.prompt_file = Some(PathBuf::from(value));
                }
            }
            "--stdin" => options.read_stdin = true,
            "--session" | "--resume" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.session_id = Some(value.clone());
                }
            }
            "--label" | "--tag" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.label = Some(value.clone());
                }
            }
            "--format" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.output = match value.as_str() {
                        "text" | "plain" => HarnessOutput::Text,
                        _ => HarnessOutput::Ndjson,
                    };
                }
            }
            "--text" | "--plain" => options.output = HarnessOutput::Text,
            "--ndjson" | "--json" => options.output = HarnessOutput::Ndjson,
            "--verify" => options.verify = true,
            "--no-verify" => options.verify = false,
            "--policy" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.policy = match value.as_str() {
                        "strict" => HarnessPolicy::Strict,
                        _ => HarnessPolicy::Standard,
                    };
                }
            }
            "--strict" => options.policy = HarnessPolicy::Strict,
            "--dry-run" => options.dry_run = true,
            "--" => {
                free.extend(args.iter().skip(index + 1).cloned());
                break;
            }
            value => free.push(value.to_string()),
        }
        index += 1;
    }

    if !free.is_empty() {
        options.prompt_parts.push(free.join(" "));
    }

    options
}

pub fn run(paths: &AppPaths, action: HarnessAction) -> Result<(), String> {
    match action {
        HarnessAction::Help => {
            print_help();
            Ok(())
        }
        HarnessAction::Doctor => doctor(paths),
        HarnessAction::Evals => print_eval_catalog(),
        HarnessAction::Replay { run_id } => replay(paths, run_id),
        HarnessAction::Inspect { run_id } => inspect(paths, run_id),
        HarnessAction::Run(options) => run_harness(paths, options),
    }
}

pub fn print_help() {
    println!("wirecli harness");
    println!();
    println!("usage:");
    println!("  wirecli harness run --prompt <task>");
    println!("  wirecli harness run --prompt-file task.md");
    println!("  wirecli harness run --stdin");
    println!("  wirecli harness replay [latest|run-id]");
    println!("  wirecli harness inspect [latest|run-id]");
    println!("  wirecli harness doctor");
    println!("  wirecli harness evals");
    println!();
    println!("run flags:");
    println!("  --session <id>       continue an existing Wire session");
    println!("  --text               stream assistant text to stdout instead of NDJSON");
    println!("  --no-verify          skip deterministic post-run verification");
    println!("  --policy strict      use the stricter command policy");
    println!("  --dry-run            emit preflight and manifest without calling the model");
}

fn run_harness(paths: &AppPaths, options: HarnessRunOptions) -> Result<(), String> {
    let user_prompt = collect_prompt(&options)?;
    if user_prompt.trim().is_empty() {
        return Err("missing harness prompt".to_string());
    }

    let config = AppConfig::load(paths)?;
    if config.requires_login() {
        return Err(
            "login required; run `wirecli login` or edit `~/.wirecli/config/config.toml`"
                .to_string(),
        );
    }
    if config.requires_model_selection() {
        return Err(
            "model not selected; run `wirecli` and choose a model with /models".to_string(),
        );
    }
    if let Some(message) = provider_model_mismatch(&config) {
        return Err(message);
    }

    let run_id = new_run_id();
    let started_at_ms = now_ms();
    let started = Instant::now();
    let logger = RunLogger::new(paths, &run_id)?;
    let mut observer = HarnessObserver::new(
        run_id.clone(),
        logger,
        options.output,
        started_at_ms,
        paths,
        &config,
    );
    let (preflight, manifest, memory_context) =
        build_preflight(paths, &config, options.policy, &user_prompt)?;

    observer.emit(json!({
        "type": "HarnessStart",
        "cwd": paths.root_dir.display().to_string(),
        "provider": config.provider_status_label(),
        "protocol": provider_protocol(&config).as_str(),
        "model": config.model.clone(),
        "policy": policy_name(options.policy),
        "label": options.label.clone(),
        "dry_run": options.dry_run
    }))?;
    observer.emit(json!({
        "type": "ToolManifest",
        "manifest": manifest
    }))?;
    observer.emit(json!({
        "type": "Preflight",
        "preflight": preflight
    }))?;

    if options.dry_run {
        let duration_ms = started.elapsed().as_millis();
        let verification = Some(HarnessVerification {
            status: "dry_run".to_string(),
            score: 100,
            findings: vec!["model call skipped by --dry-run".to_string()],
            tool_calls: 0,
            edited: false,
            validation_seen: false,
            duration_ms,
        });
        observer.emit(json!({
            "type": "Result",
            "success": true,
            "dry_run": true,
            "duration_ms": duration_ms
        }))?;
        write_metadata(
            paths,
            HarnessMetadata {
                run_id: run_id.clone(),
                session_id: observer.session_id.clone(),
                project_key: paths.project_key.clone(),
                cwd: paths.root_dir.display().to_string(),
                provider: config.provider_status_label(),
                protocol: provider_protocol(&config).as_str().to_string(),
                model: config.model.clone(),
                label: options.label.clone(),
                started_at_ms,
                duration_ms,
                success: true,
                dry_run: true,
                prompt_preview: preview(&user_prompt, 240),
                log_path: observer.log_path().display().to_string(),
                event_counts: observer.event_counts.clone(),
                verification,
            },
        )?;
        return Ok(());
    }

    let harness_prompt = build_harness_prompt(&user_prompt, &memory_context, options.verify);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let command_policy = match options.policy {
        HarnessPolicy::Standard => CommandPolicy::standard(),
        HarnessPolicy::Strict => CommandPolicy::strict(),
    };
    let result = runtime.block_on(responses_agent::run_prompt_input_in_session_with_observer(
        paths,
        &config,
        options.session_id.clone(),
        PromptInput::text(harness_prompt),
        AgentControl::default(),
        command_policy,
        &mut observer,
    ));
    runtime.shutdown_background();

    let duration_ms = started.elapsed().as_millis();
    match result {
        Ok((session_id, output)) => {
            if options.output == HarnessOutput::Text && !output.ends_with('\n') {
                println!();
            }
            observer.emit(json!({
                "type": "Message",
                "role": "assistant",
                "text": output.clone()
            }))?;
            let verification = if options.verify {
                let verification =
                    verify_run(paths, &session_id, &user_prompt, &observer, duration_ms);
                observer.emit(json!({
                    "type": "Verification",
                    "verification": verification
                }))?;
                Some(verification)
            } else {
                None
            };
            observer.emit(json!({
                "type": "Result",
                "success": true,
                "session_id": session_id.clone(),
                "duration_ms": duration_ms,
                "log_path": observer.log_path().display().to_string()
            }))?;
            write_metadata(
                paths,
                HarnessMetadata {
                    run_id,
                    session_id: Some(session_id),
                    project_key: paths.project_key.clone(),
                    cwd: paths.root_dir.display().to_string(),
                    provider: config.provider_status_label(),
                    protocol: provider_protocol(&config).as_str().to_string(),
                    model: config.model.clone(),
                    label: options.label.clone(),
                    started_at_ms,
                    duration_ms,
                    success: true,
                    dry_run: false,
                    prompt_preview: preview(&user_prompt, 240),
                    log_path: observer.log_path().display().to_string(),
                    event_counts: observer.event_counts.clone(),
                    verification,
                },
            )?;
            Ok(())
        }
        Err(err) => {
            observer.emit(json!({
                "type": "Error",
                "message": err.clone(),
                "code": "agent_error"
            }))?;
            observer.emit(json!({
                "type": "Result",
                "success": false,
                "duration_ms": duration_ms,
                "log_path": observer.log_path().display().to_string()
            }))?;
            write_metadata(
                paths,
                HarnessMetadata {
                    run_id,
                    session_id: observer.session_id.clone(),
                    project_key: paths.project_key.clone(),
                    cwd: paths.root_dir.display().to_string(),
                    provider: config.provider_status_label(),
                    protocol: provider_protocol(&config).as_str().to_string(),
                    model: config.model.clone(),
                    label: options.label.clone(),
                    started_at_ms,
                    duration_ms,
                    success: false,
                    dry_run: false,
                    prompt_preview: preview(&user_prompt, 240),
                    log_path: observer.log_path().display().to_string(),
                    event_counts: observer.event_counts.clone(),
                    verification: None,
                },
            )?;
            Err(err)
        }
    }
}

fn print_eval_catalog() -> Result<(), String> {
    let suites = eval_catalog();
    let total = suites.iter().map(|suite| suite.count).sum::<usize>();
    println!("Wire Harness eval catalog");
    println!("total_tasks: {total}");
    for suite in suites {
        println!(
            "- {}: {} tasks | {} | metrics: {}",
            suite.id,
            suite.count,
            suite.description,
            suite.metrics.join(", ")
        );
    }
    println!("pass_criteria:");
    println!("- completed task without fabricated tools or paths");
    println!("- build/test status is explicit and backed by timeline evidence");
    println!("- command denials, sandbox blocks, and provider parser anomalies are recoverable");
    println!("- no sandbox escape, secret persistence, or removed-feature regression");
    Ok(())
}

#[derive(Debug, Clone)]
struct EvalSuite {
    id: &'static str,
    count: usize,
    description: &'static str,
    metrics: &'static [&'static str],
}

fn eval_catalog() -> Vec<EvalSuite> {
    vec![
        EvalSuite {
            id: "bugs-real",
            count: 50,
            description: "real bug-fix tasks with failing evidence and expected validator",
            metrics: &["completed", "build_ok", "tests_run", "regression_fixed"],
        },
        EvalSuite {
            id: "refactors",
            count: 50,
            description: "scoped refactors that must preserve behavior and repo style",
            metrics: &["completed", "diff_scope", "tests_run", "no_unrelated_churn"],
        },
        EvalSuite {
            id: "large-repo",
            count: 50,
            description: "large-repository navigation and targeted patch tasks",
            metrics: &[
                "files_inspected",
                "latency_ms",
                "context_compacted",
                "completed",
            ],
        },
        EvalSuite {
            id: "tool-call-bugs",
            count: 20,
            description: "malformed, repeated, unknown, and provider-fragmented tool calls",
            metrics: &[
                "acp_recovered",
                "wdf_repair_count",
                "no_agent_stop",
                "completed",
            ],
        },
        EvalSuite {
            id: "permission-sandbox",
            count: 20,
            description: "commands requiring approval, denials, network blocks, and path escapes",
            metrics: &[
                "approval_logged",
                "sandbox_enforced",
                "no_escape",
                "safe_alternative",
            ],
        },
    ]
}

fn collect_prompt(options: &HarnessRunOptions) -> Result<String, String> {
    let mut parts = Vec::new();
    parts.extend(options.prompt_parts.iter().cloned());
    if let Some(path) = &options.prompt_file {
        parts.push(fs::read_to_string(path).map_err(|e| e.to_string())?);
    }
    if options.read_stdin || (!io::stdin().is_terminal() && parts.is_empty()) {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| e.to_string())?;
        parts.push(input);
    }
    Ok(parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn build_preflight(
    paths: &AppPaths,
    config: &AppConfig,
    policy: HarnessPolicy,
    prompt: &str,
) -> Result<(HarnessPreflight, ToolManifest, Vec<String>), String> {
    let registry = McpRegistry::load(paths)?;
    let report = registry.discover_tools_report();
    let mut memory_context = Vec::new();
    if let Ok(store) = MemoryContextStore::new(paths) {
        if let Ok(Some(block)) = store.render_compact(&paths.project_key, prompt, 5) {
            memory_context.push(block);
        }
    }
    if let Ok(store) = LabStore::new(paths) {
        if let Ok(Some(block)) = store.render_compact(&paths.project_key, prompt, 5) {
            memory_context.push(block);
        }
    }
    let provider = active_provider(config);
    let mcp_tools = report
        .tools
        .iter()
        .map(|tool| {
            format!(
                "{}:{}:{}",
                tool.function_name, tool.server_name, tool.tool_name
            )
        })
        .collect::<Vec<_>>();
    let manifest = ToolManifest {
        builtin_tools: BOX_TOOL_NAMES.iter().map(|tool| tool.to_string()).collect(),
        mcp_servers: registry.servers().len(),
        mcp_tools: mcp_tools.clone(),
        mcp_warnings: report.errors.clone(),
    };
    let preflight = HarnessPreflight {
        cwd: paths.root_dir.display().to_string(),
        provider: config.provider_status_label(),
        protocol: provider_protocol(config).as_str().to_string(),
        model: config.model.clone(),
        permission_mode: config.permission_mode.title().to_string(),
        policy: policy_name(policy).to_string(),
        tools_builtin: BOX_TOOL_NAMES.len(),
        mcp_servers: registry.servers().len(),
        mcp_tools: mcp_tools.len(),
        mcp_warnings: report.errors,
        memory_blocks: memory_context.len(),
        git: git_summary(&paths.root_dir),
    };
    let mut context = Vec::new();
    context.push(format!(
        "Provider: {} / {} / model {}.",
        preflight.provider, preflight.protocol, preflight.model
    ));
    context.push(format!(
        "Tool surface: {} built-in tools, {} MCP tools. Parallel tool calls: {}.",
        preflight.tools_builtin, preflight.mcp_tools, provider.supports_parallel_tool_calls
    ));
    if let Some(git) = &preflight.git {
        context.push(format!(
            "Git: branch {}, {} dirty files before run.",
            git.branch.as_deref().unwrap_or("unknown"),
            git.dirty_files
        ));
    }
    context.extend(memory_context);
    Ok((preflight, manifest, context))
}

fn build_harness_prompt(user_prompt: &str, memory_context: &[String], verify: bool) -> String {
    let mut prompt = String::new();
    prompt.push_str("User request:\n");
    prompt.push_str(user_prompt.trim());
    prompt.push_str("\n\nWire Harness operating contract:\n");
    prompt.push_str("- Treat this as an end-to-end delivery run, not a planning-only chat.\n");
    prompt.push_str("- Keep a concise plan current for non-trivial work and move through implementation, evidence gathering, and final report.\n");
    prompt.push_str("- Use repository inspection before making claims about files, commands, tools, or architecture.\n");
    prompt.push_str("- Prefer focused patches and the existing Wire CLI tool surface. Do not invent unavailable tools.\n");
    prompt
        .push_str("- Separate implementation status from validation status in the final answer.\n");
    if verify {
        prompt.push_str("- Before the final answer, run the most relevant cheap validation available for files you changed, or explicitly state why validation could not run.\n");
    }
    if !memory_context.is_empty() {
        prompt.push_str("\nHarness context snapshot:\n");
        for block in memory_context {
            prompt.push_str(block.trim());
            prompt.push('\n');
        }
    }
    prompt
}

fn verify_run(
    paths: &AppPaths,
    session_id: &str,
    user_prompt: &str,
    observer: &HarnessObserver,
    duration_ms: u128,
) -> HarnessVerification {
    let timeline = SessionStore::new(paths)
        .and_then(|store| store.timeline(&paths.project_key, session_id))
        .unwrap_or_default();
    let tool_calls = timeline
        .iter()
        .filter(|event| {
            event
                .command
                .as_deref()
                .map(|command| command.starts_with("tool.call "))
                .unwrap_or(false)
        })
        .count();
    let edited = timeline.iter().any(event_is_edit);
    let validation_seen = timeline.iter().any(event_has_validation);
    let mut findings = Vec::new();
    if observer.text_buffer.trim().is_empty() {
        findings.push("assistant returned no visible final text".to_string());
    }
    if task_likely_needs_tools(user_prompt) && tool_calls == 0 {
        findings.push("task looked implementation/research oriented but no tools ran".to_string());
    }
    if edited && !validation_seen {
        findings.push(
            "files appear to have been edited but no validation command was detected".to_string(),
        );
    }
    if timeline.iter().any(event_has_error_signal) {
        findings.push(
            "timeline contains error-like tool or provider output; inspect the run log".to_string(),
        );
    }
    if observer.repeated_tool_names() {
        findings.push(
            "same tool was invoked many times; review for loop or inefficient exploration"
                .to_string(),
        );
    }

    let mut score = 100u8;
    for finding in &findings {
        if finding.contains("no visible") {
            score = score.saturating_sub(60);
        } else if finding.contains("no validation") {
            score = score.saturating_sub(22);
        } else if finding.contains("no tools") {
            score = score.saturating_sub(18);
        } else {
            score = score.saturating_sub(10);
        }
    }
    let status = if score >= 86 {
        "passed"
    } else if score >= 65 {
        "warn"
    } else {
        "failed"
    }
    .to_string();

    HarnessVerification {
        status,
        score,
        findings,
        tool_calls,
        edited,
        validation_seen,
        duration_ms,
    }
}

fn event_is_edit(event: &TimelineEvent) -> bool {
    let command = event.command.as_deref().unwrap_or_default();
    command.contains("tool.call apply_patch")
        || command.contains("tool.call write_file")
        || command.contains("tool.call replace_in_file")
        || command.contains("tool.call delete_file")
        || command.contains("tool.call copy_file")
        || command.contains("tool.call move_file")
}

fn event_has_validation(event: &TimelineEvent) -> bool {
    let haystack = format!(
        "{}\n{}",
        event.command.as_deref().unwrap_or_default(),
        event.stdout.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    [
        "cargo check",
        "cargo test",
        "rustfmt",
        "npm test",
        "npm run",
        "pnpm test",
        "pnpm run",
        "yarn test",
        "pytest",
        "go test",
        "mvn test",
        "gradle test",
        "tsc",
        "eslint",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

fn event_has_error_signal(event: &TimelineEvent) -> bool {
    if matches!(event.exit_code, Some(code) if code != 0) {
        return true;
    }
    let haystack = format!(
        "{}\n{}\n{}",
        event.content.as_deref().unwrap_or_default(),
        event.stdout.as_deref().unwrap_or_default(),
        event.stderr.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    ["error:", "failed", "panic", "traceback", "provider error"]
        .iter()
        .any(|needle| haystack.contains(needle))
}

fn task_likely_needs_tools(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    [
        "implemente",
        "implement",
        "corrige",
        "fix",
        "debug",
        "pesquisa",
        "research",
        "verifique",
        "test",
        "arquivo",
        "repo",
        "cli",
        "codigo",
        "codigo",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn doctor(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    println!("Wire Harness doctor");
    println!("cwd: {}", paths.root_dir.display());
    println!("provider: {}", config.provider_status_label());
    println!("protocol: {}", provider_protocol(&config).as_str());
    println!(
        "model: {}",
        if config.model.trim().is_empty() {
            "not selected"
        } else {
            config.model.as_str()
        }
    );
    println!(
        "login: {}",
        if config.requires_login() {
            "required"
        } else {
            "ok"
        }
    );
    println!("permission mode: {}", config.permission_mode.title());
    println!("built-in tools: {}", BOX_TOOL_NAMES.len());
    let registry = McpRegistry::load(paths)?;
    let report = registry.discover_tools_report();
    println!("mcp servers: {}", registry.servers().len());
    println!("mcp tools: {}", report.tools.len());
    for error in report.errors {
        println!("mcp warning: {error}");
    }
    if let Some(git) = git_summary(&paths.root_dir) {
        println!(
            "git: branch {} dirty_files {}",
            git.branch.as_deref().unwrap_or("unknown"),
            git.dirty_files
        );
    }
    println!("runs: {}", run_dir(paths).display());
    Ok(())
}

fn replay(paths: &AppPaths, run_id: Option<String>) -> Result<(), String> {
    let run_id = resolve_run_id(paths, run_id)?;
    let path = run_path(paths, &run_id);
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    print!("{raw}");
    Ok(())
}

fn inspect(paths: &AppPaths, run_id: Option<String>) -> Result<(), String> {
    let run_id = resolve_run_id(paths, run_id)?;
    let metadata = read_metadata(paths, &run_id).ok();
    let path = run_path(paths, &run_id);
    println!("run: {run_id}");
    println!("log: {}", path.display());
    if let Some(metadata) = metadata {
        println!("success: {}", metadata.success);
        println!(
            "session: {}",
            metadata.session_id.as_deref().unwrap_or("unknown")
        );
        println!("provider: {} {}", metadata.provider, metadata.model);
        println!("duration_ms: {}", metadata.duration_ms);
        if let Some(verification) = metadata.verification {
            println!(
                "verification: {} score {}",
                verification.status, verification.score
            );
            for finding in verification.findings {
                println!("- {finding}");
            }
        }
        return Ok(());
    }

    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_result = None;
    for line in raw.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(event_type) = value.get("type").and_then(|value| value.as_str()) {
                *counts.entry(event_type.to_string()).or_default() += 1;
                if event_type == "Result" {
                    last_result = Some(value);
                }
            }
        }
    }
    println!("events:");
    for (event_type, count) in counts {
        println!("- {event_type}: {count}");
    }
    if let Some(result) = last_result {
        println!(
            "result: {}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
    }
    Ok(())
}

struct HarnessObserver {
    run_id: String,
    logger: RunLogger,
    output: HarnessOutput,
    started_at_ms: u128,
    session_id: Option<String>,
    text_buffer: String,
    event_counts: BTreeMap<String, u64>,
    active_tools: HashMap<String, ToolStart>,
    tool_name_counts: BTreeMap<String, u64>,
    usage: TokenUsage,
}

impl HarnessObserver {
    fn new(
        run_id: String,
        logger: RunLogger,
        output: HarnessOutput,
        started_at_ms: u128,
        paths: &AppPaths,
        config: &AppConfig,
    ) -> Self {
        let mut observer = Self {
            run_id,
            logger,
            output,
            started_at_ms,
            session_id: None,
            text_buffer: String::new(),
            event_counts: BTreeMap::new(),
            active_tools: HashMap::new(),
            tool_name_counts: BTreeMap::new(),
            usage: TokenUsage::default(),
        };
        let _ = observer.emit(json!({
            "type": "RunAllocated",
            "cwd": paths.root_dir.display().to_string(),
            "provider": config.provider_status_label(),
            "protocol": provider_protocol(config).as_str(),
            "model": config.model.clone()
        }));
        observer
    }

    fn emit(&mut self, mut event: Value) -> Result<(), String> {
        let event_type = event
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("Event")
            .to_string();
        if let Some(object) = event.as_object_mut() {
            object
                .entry("timestamp_ms".to_string())
                .or_insert_with(|| json!(now_ms()));
            object
                .entry("run_id".to_string())
                .or_insert_with(|| json!(self.run_id.clone()));
            if let Some(session_id) = &self.session_id {
                object
                    .entry("session_id".to_string())
                    .or_insert_with(|| json!(session_id));
            }
        }
        redact_value(&mut event);
        *self.event_counts.entry(event_type).or_default() += 1;
        self.logger.append(&event)?;
        if self.output == HarnessOutput::Ndjson {
            println!(
                "{}",
                serde_json::to_string(&event).map_err(|e| e.to_string())?
            );
        }
        Ok(())
    }

    fn log_path(&self) -> &Path {
        &self.logger.path
    }

    fn repeated_tool_names(&self) -> bool {
        self.tool_name_counts.values().any(|count| *count >= 8)
    }
}

impl AgentObserver for HarnessObserver {
    fn on_event(&mut self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::SessionBound(session_id) => {
                self.session_id = Some(session_id.to_string());
                let _ = self.emit(json!({
                    "type": "SessionStart",
                    "session_id": session_id,
                    "agent": "wirecli",
                    "timestamp_since_run_ms": now_ms().saturating_sub(self.started_at_ms)
                }));
            }
            AgentEvent::TextDelta(delta) => {
                self.text_buffer.push_str(delta);
                if self.output == HarnessOutput::Text {
                    print!("{delta}");
                    let _ = io::stdout().flush();
                }
                let _ = self.emit(json!({
                    "type": "TextDelta",
                    "text": delta
                }));
            }
            AgentEvent::ToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                let (arguments_delta, truncated) = truncate_event_text(arguments_delta, 8_000);
                let _ = self.emit(json!({
                    "type": "ToolDelta",
                    "call_id": call_id,
                    "tool_name": name,
                    "arguments_delta": arguments_delta,
                    "arguments_truncated": truncated
                }));
            }
            AgentEvent::Status(status) => {
                let _ = self.emit(json!({
                    "type": "Status",
                    "message": status
                }));
            }
            AgentEvent::ToolCallStart {
                call_id,
                name,
                arguments,
                summary,
            } => {
                self.active_tools.insert(
                    call_id.to_string(),
                    ToolStart {
                        name: name.to_string(),
                        started_at: Instant::now(),
                    },
                );
                *self.tool_name_counts.entry(name.to_string()).or_default() += 1;
                let _ = self.emit(json!({
                    "type": "ToolStart",
                    "call_id": call_id,
                    "tool_name": name,
                    "input": arguments,
                    "summary": summary
                }));
            }
            AgentEvent::ToolCallResult {
                call_id,
                name,
                output,
            } => {
                let duration_ms = self
                    .active_tools
                    .remove(call_id)
                    .map(|start| {
                        let _tool_name = start.name;
                        start.started_at.elapsed().as_millis()
                    })
                    .unwrap_or_default();
                let (output, truncated) = truncate_event_text(output, 32_000);
                let _ = self.emit(json!({
                    "type": "ToolEnd",
                    "call_id": call_id,
                    "tool_name": name,
                    "success": true,
                    "duration_ms": duration_ms,
                    "output": output,
                    "output_truncated": truncated
                }));
            }
            AgentEvent::Usage(usage) => {
                self.usage = usage.clone();
                let _ = self.emit(json!({
                    "type": "UsageDelta",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "total_tokens": usage.total_tokens
                }));
            }
        }
    }
}

struct RunLogger {
    path: PathBuf,
    file: File,
}

impl RunLogger {
    fn new(paths: &AppPaths, run_id: &str) -> Result<Self, String> {
        let dir = run_dir(paths);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{run_id}.ndjson"));
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| e.to_string())?
        };
        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        Ok(Self { path, file })
    }

    fn append(&mut self, value: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.file, value).map_err(|e| e.to_string())?;
        self.file.write_all(b"\n").map_err(|e| e.to_string())?;
        self.file.flush().map_err(|e| e.to_string())
    }
}

fn write_metadata(paths: &AppPaths, metadata: HarnessMetadata) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(&metadata).map_err(|e| e.to_string())?;
    write_private_file(&metadata_path(paths, &metadata.run_id), &raw)
}

fn read_metadata(paths: &AppPaths, run_id: &str) -> Result<HarnessMetadata, String> {
    let raw = fs::read_to_string(metadata_path(paths, run_id)).map_err(|e| e.to_string())?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

fn resolve_run_id(paths: &AppPaths, run_id: Option<String>) -> Result<String, String> {
    match run_id.as_deref() {
        Some("") | None | Some("latest") => latest_run_id(paths),
        Some(value) => Ok(value.to_string()),
    }
}

fn latest_run_id(paths: &AppPaths) -> Result<String, String> {
    let dir = run_dir(paths);
    if !dir.exists() {
        return Err("no harness runs found".to_string());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("ndjson") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            candidates.push((modified, stem.to_string()));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, run_id)| run_id)
        .ok_or_else(|| "no harness runs found".to_string())
}

fn run_dir(paths: &AppPaths) -> PathBuf {
    paths.data_dir.join("harness").join("runs")
}

fn run_path(paths: &AppPaths, run_id: &str) -> PathBuf {
    run_dir(paths).join(format!("{run_id}.ndjson"))
}

fn metadata_path(paths: &AppPaths, run_id: &str) -> PathBuf {
    run_dir(paths).join(format!("{run_id}.json"))
}

fn new_run_id() -> String {
    let id = next_id();
    format!("hrn_{}", &id[..24])
}

fn policy_name(policy: HarnessPolicy) -> &'static str {
    match policy {
        HarnessPolicy::Standard => "standard",
        HarnessPolicy::Strict => "strict",
    }
}

fn git_summary(root: &Path) -> Option<GitSummary> {
    if !root.join(".git").exists() {
        return None;
    }
    let output = Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = None;
    let mut dirty_files = 0usize;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            branch = Some(rest.to_string());
        } else if !line.trim().is_empty() {
            dirty_files = dirty_files.saturating_add(1);
        }
    }
    Some(GitSummary {
        branch,
        dirty_files,
    })
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = sanitize_event_string(text);
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                redact_value(value);
            }
        }
        _ => {}
    }
}

fn sanitize_event_string(text: &str) -> String {
    let text = redact_secrets(text);
    const MAX_EVENT_STRING_CHARS: usize = 64_000;
    if text.chars().count() <= MAX_EVENT_STRING_CHARS {
        return text;
    }
    let mut out = text
        .chars()
        .take(MAX_EVENT_STRING_CHARS.saturating_sub(20))
        .collect::<String>();
    out.push_str("\n[truncated]");
    out
}

fn truncate_event_text(text: &str, max_chars: usize) -> (String, bool) {
    let text = redact_secrets(text);
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let mut out = text
        .chars()
        .take(max_chars.saturating_sub(20))
        .collect::<String>();
    out.push_str("\n[truncated]");
    (out, true)
}

fn preview(text: &str, max_chars: usize) -> String {
    let text = redact_secrets(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut out = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_harness_run_prompt() {
        let action = parse_action(&[
            "run".to_string(),
            "--prompt".to_string(),
            "ship it".to_string(),
            "--text".to_string(),
        ]);
        match action {
            HarnessAction::Run(options) => {
                assert_eq!(options.prompt_parts, vec!["ship it"]);
                assert_eq!(options.output, HarnessOutput::Text);
                assert!(options.verify);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn validation_detector_recognizes_cargo_check() {
        let event = TimelineEvent {
            kind: "command".to_string(),
            role: None,
            content: Some("ok".to_string()),
            command: Some("tool.call shell".to_string()),
            stdout: Some("Shell\n```bash\ncargo check --offline\n```".to_string()),
            stderr: None,
            exit_code: Some(0),
            created_at: "now".to_string(),
        };
        assert!(event_has_validation(&event));
    }

    #[test]
    fn parse_harness_evals_command() {
        match parse_action(&["evals".to_string()]) {
            HarnessAction::Evals => {}
            _ => panic!("expected evals action"),
        }
        assert_eq!(
            eval_catalog()
                .iter()
                .map(|suite| suite.count)
                .sum::<usize>(),
            190
        );
    }
}
