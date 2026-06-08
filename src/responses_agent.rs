use crate::acp::{self, AcpPhase};
use crate::agent_tools::{BoxTools, BOX_TOOL_NAMES};
use crate::commands::parser::split_command_line;
use crate::config::{AppConfig, AppPaths, PermissionMode};
use crate::context::Loom;
use crate::hooks::{HookContext, HookExecution, HookStore};
use crate::lab::{LabInput, LabStore};
use crate::mcp::{McpRegistry, McpToolSpec};
use crate::memory::{AnchorInput, AnchorStore};
use crate::memory_context::MemoryContextStore;
use crate::model_catalog;
use crate::models::endpoint_error_message;
use crate::policy::CommandPolicy;
use crate::prompt::base_developer_prompt;
use crate::providers::{
    active_provider, provider_headers, provider_is_stateless, provider_model_mismatch,
    provider_protocol, ProviderProtocol,
};
use crate::safekey::redact_secrets;
use crate::sandbox::SandboxManager;
use crate::session::{SessionEvent, SessionStore};
use crate::skills::SkillStore;
use crate::subagents::{run_subagent, SubagentRole};
use crate::verifier::{VerifierPipeline, VerifierReport};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::str;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

const MAX_AGENT_BACKEND_TURNS: usize = 48;
const MAX_EMPTY_TOOL_CONTINUATIONS: usize = 2;
const MAX_REPEATED_IDENTICAL_TOOL_TURNS: usize = 2;
const MAX_WDF_PARSER_RECOVERIES: usize = 1;
const MAX_PROVIDER_REQUEST_RETRIES_AFTER_TOOLS: usize = 1;
const PROVIDER_REQUEST_RETRY_DELAY_MS: u64 = 800;

#[derive(Debug, Clone)]
struct ToolCall {
    call_id: String,
    name: String,
    arguments: Value,
}

#[derive(Debug, Clone)]
struct ToolDispatchResult {
    model_output: String,
    ui_output: String,
}

#[derive(Debug, Clone)]
struct CompletedToolCall {
    call: ToolCall,
    result: ToolDispatchResult,
    summary: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct PromptImage {
    pub label: String,
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone)]
pub struct PromptInput {
    pub text: String,
    pub images: Vec<PromptImage>,
}

impl PromptInput {
    pub fn text(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentControl {
    cancelled: Arc<AtomicBool>,
}

impl AgentControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("cancelled".to_string())
        } else {
            Ok(())
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct TurnResult {
    response_id: Option<String>,
    text: Option<String>,
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Default)]
struct StreamTurn {
    response_id: Option<String>,
    text: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<TokenUsage>,
}

#[allow(dead_code)]
pub enum AgentEvent<'a> {
    SessionBound(&'a str),
    TextDelta(&'a str),
    ToolCallDelta {
        call_id: Option<&'a str>,
        name: Option<&'a str>,
        arguments_delta: &'a str,
    },
    Status(&'a str),
    ToolCallStart {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a Value,
        summary: &'a str,
    },
    ToolCallResult {
        call_id: &'a str,
        name: &'a str,
        output: &'a str,
    },
    Usage(TokenUsage),
}

pub trait AgentObserver {
    fn on_event(&mut self, event: AgentEvent<'_>);
}

pub struct ConsoleObserver;

impl AgentObserver for ConsoleObserver {
    fn on_event(&mut self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::TextDelta(delta) => {
                print!("{delta}");
                let _ = io::stdout().flush();
            }
            AgentEvent::SessionBound(_) => {}
            _ => {}
        }
    }
}

pub async fn run_prompt(
    paths: &AppPaths,
    config: &AppConfig,
    prompt: String,
) -> Result<(String, String), String> {
    let mut observer = ConsoleObserver;
    run_prompt_with_observer(paths, config, prompt, &mut observer).await
}

pub async fn complete_text(config: &AppConfig, instructions: String) -> Result<String, String> {
    complete_text_with_model(config, &config.model, instructions).await
}

pub async fn complete_text_with_model(
    config: &AppConfig,
    model: &str,
    instructions: String,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    if provider_protocol(config) == ProviderProtocol::ChatCompletions {
        let mut body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": instructions },
                { "role": "user", "content": "Return the requested text now." }
            ],
            "stream": false,
        });
        apply_reasoning_effort(&mut body, config);

        let response = client
            .post(format!(
                "{}/chat/completions",
                config.base_url.trim_end_matches('/')
            ))
            .header(ACCEPT_ENCODING, "identity")
            .headers(provider_headers(config)?)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(endpoint_error_message(
                "chat completions endpoint",
                status.as_u16(),
                &value,
                "login required",
            ));
        }
        return value
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .map(|content| content.to_string())
            .ok_or_else(|| "chat completions upstream returned no text".to_string());
    }
    if provider_protocol(config) == ProviderProtocol::AnthropicMessages {
        let body = json!({
            "model": model,
            "system": instructions,
            "messages": [
                { "role": "user", "content": "Return the requested text now." }
            ],
            "max_tokens": 4096,
            "stream": false,
        });

        let response = client
            .post(format!(
                "{}/messages",
                config.base_url.trim_end_matches('/')
            ))
            .header(ACCEPT_ENCODING, "identity")
            .headers(provider_headers(config)?)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(endpoint_error_message(
                "anthropic messages endpoint",
                status.as_u16(),
                &value,
                "login required",
            ));
        }
        return value
            .get("content")
            .and_then(|content| content.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(|text| text.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "anthropic messages upstream returned no text".to_string());
    }

    let body = json!({
        "model": model,
        "instructions": instructions,
        "input": "",
        "tools": [],
        "parallel_tool_calls": false,
        "stream": false,
        "store": false,
    });

    let response = client
        .post(format!(
            "{}/responses",
            config.base_url.trim_end_matches('/')
        ))
        .header(ACCEPT_ENCODING, "identity")
        .headers(provider_headers(config)?)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    let value: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(endpoint_error_message(
            "responses endpoint",
            status.as_u16(),
            &value,
            "login required",
        ));
    }

    parse_turn(value)?
        .text
        .ok_or_else(|| "responses upstream returned no text".to_string())
}

pub async fn run_prompt_with_observer(
    paths: &AppPaths,
    config: &AppConfig,
    prompt: String,
    observer: &mut dyn AgentObserver,
) -> Result<(String, String), String> {
    run_prompt_input_with_observer(paths, config, PromptInput::text(prompt), observer).await
}

pub async fn run_prompt_input_with_observer(
    paths: &AppPaths,
    config: &AppConfig,
    input: PromptInput,
    observer: &mut dyn AgentObserver,
) -> Result<(String, String), String> {
    run_prompt_input_in_session_with_observer(
        paths,
        config,
        None,
        input,
        AgentControl::default(),
        CommandPolicy::standard(),
        observer,
    )
    .await
}

pub async fn run_prompt_input_in_session_with_observer(
    paths: &AppPaths,
    config: &AppConfig,
    session_id: Option<String>,
    input: PromptInput,
    control: AgentControl,
    command_policy: CommandPolicy,
    observer: &mut dyn AgentObserver,
) -> Result<(String, String), String> {
    let mut store = SessionStore::new(paths)?;
    let (session_id, is_new_session) = match session_id {
        Some(id) => {
            let session = store.resolve(&paths.project_key, Some(id))?;
            (session.id, false)
        }
        None => {
            let session = store.create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                &input.text,
            )?;
            (session.id, true)
        }
    };
    if is_new_session {
        store.append_event(
            &paths.project_key,
            &session_id,
            SessionEvent::developer(base_developer_prompt(paths)),
        )?;
    }
    observer.on_event(AgentEvent::SessionBound(&session_id));
    store.append_event(
        &paths.project_key,
        &session_id,
        SessionEvent::user(input.text.clone()),
    )?;
    maybe_store_language_preference(
        paths,
        config,
        &mut store,
        &paths.project_key,
        &session_id,
        &input.text,
    );
    maybe_observe_lab(paths, config, &paths.project_key, &session_id, &input.text);
    maybe_offer_memory_or_skill_suggestion(
        config,
        &mut store,
        &paths.project_key,
        &session_id,
        &input.text,
    );

    let sandbox = SandboxManager::new(paths)?;
    let box_summary = sandbox.create(&format!("session-{}", session_id))?;
    store.append_command(
        &paths.project_key,
        &session_id,
        &["box.create".to_string(), box_summary.id.clone()],
        "ok",
        Some(0),
        &format!(
            "box={} workspace={}",
            box_summary.id,
            sandbox.workspace_path(&box_summary.id).display()
        ),
        "",
    )?;

    let anchors = AnchorStore::new(paths)?;
    let hooks = HookStore::new(paths)?;
    let toolbox = BoxTools::new(
        &sandbox,
        &anchors,
        &hooks,
        config,
        config.permission_mode,
        command_policy,
    );
    run_lifecycle_hooks(
        paths,
        &toolbox,
        &mut store,
        &paths.project_key,
        &session_id,
        &box_summary.id,
        "session_start",
        HookContext::default()
            .session(&session_id)
            .status(if is_new_session { "new" } else { "resumed" })
            .payload(&format!(
                "model={} provider={}",
                config.model, config.provider
            )),
    );
    let mcp_registry = McpRegistry::load(paths)?;
    observer.on_event(AgentEvent::Status("discovering mcp tools"));
    let mcp_report = discover_mcp_tools_blocking(mcp_registry.clone()).await;
    for error in &mcp_report.errors {
        observer.on_event(AgentEvent::Status(&format!("mcp warning: {error}")));
    }
    let mcp_tools = mcp_report.tools;
    let loom = Loom::new(paths)?;
    observer.on_event(AgentEvent::Status("loading model context window"));
    let model_info = model_catalog::load_models(config)
        .await
        .ok()
        .and_then(|models| model_catalog::current_model_info(&models, &config.model));
    observer.on_event(AgentEvent::Status("building context"));
    let mut bundle = loom.build(
        paths,
        config,
        &store,
        &session_id,
        &input.text,
        model_info.as_ref(),
    )?;
    observer.on_event(AgentEvent::Status(&bundle.context_status.label()));
    if bundle.compacted {
        observer.on_event(AgentEvent::Status(
            "auto compacting context with openrouter/free",
        ));
        run_lifecycle_hooks(
            paths,
            &toolbox,
            &mut store,
            &paths.project_key,
            &session_id,
            &box_summary.id,
            "pre_compact",
            HookContext::default()
                .session(&session_id)
                .status("pending")
                .payload(&bundle.context_status.label()),
        );
        let _ = loom
            .maybe_refresh_summary(paths, config, &store, &session_id, &bundle.context_status)
            .await;
        bundle = loom.build(
            paths,
            config,
            &store,
            &session_id,
            &input.text,
            model_info.as_ref(),
        )?;
        observer.on_event(AgentEvent::Status(&bundle.context_status.label()));
    }
    let provider_input = responses_input_value(&bundle.rendered_prompt, &input.images);
    let output = match run_agent_loop(
        paths,
        config,
        provider_input,
        &toolbox,
        &box_summary.id,
        &mcp_registry,
        &mcp_tools,
        &mut store,
        &paths.project_key,
        &session_id,
        &control,
        observer,
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            run_lifecycle_hooks(
                paths,
                &toolbox,
                &mut store,
                &paths.project_key,
                &session_id,
                &box_summary.id,
                "stop_failure",
                HookContext::default()
                    .session(&session_id)
                    .status(if err == "cancelled" {
                        "cancelled"
                    } else {
                        "error"
                    })
                    .reason(&err),
            );
            if err != "cancelled" {
                let session_error = provider_error_session_text(&err);
                let _ = store.append_command(
                    &paths.project_key,
                    &session_id,
                    &["responses.agent".to_string(), config.model.clone()],
                    "error",
                    Some(1),
                    "",
                    &err,
                );
                let _ = store.append_event(
                    &paths.project_key,
                    &session_id,
                    SessionEvent::assistant(session_error),
                );
            }
            return Err(err);
        }
    };

    store.append_command(
        &paths.project_key,
        &session_id,
        &["responses.agent".to_string(), config.model.clone()],
        "ok",
        Some(0),
        &output,
        "",
    )?;
    store.append_event(
        &paths.project_key,
        &session_id,
        SessionEvent::assistant(output.clone()),
    )?;
    run_lifecycle_hooks(
        paths,
        &toolbox,
        &mut store,
        &paths.project_key,
        &session_id,
        &box_summary.id,
        "stop",
        HookContext::default()
            .session(&session_id)
            .status("ok")
            .payload("agent completed with assistant output"),
    );

    Ok((session_id, output))
}

async fn run_agent_loop(
    paths: &AppPaths,
    config: &AppConfig,
    first_input: Value,
    toolbox: &BoxTools<'_>,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    control: &AgentControl,
    observer: &mut dyn AgentObserver,
) -> Result<String, String> {
    if let Some(message) = provider_model_mismatch(config) {
        return Err(message);
    }
    let upstream_url = config.base_url.clone();
    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let provider = active_provider(config);
    let instructions = format!(
        "{}\n\n{}\n\n{}",
        base_developer_prompt(paths),
        permission_mode_instructions(config.permission_mode),
        dynamic_tooling_instructions(paths, mcp_tools)
    );
    let tools = tool_definitions(paths, mcp_tools);
    if provider_protocol(config) == ProviderProtocol::ChatCompletions {
        return run_chat_agent_loop(
            paths,
            config,
            first_input,
            toolbox,
            box_id,
            mcp_registry,
            mcp_tools,
            store,
            project_key,
            session_id,
            control,
            observer,
            instructions,
        )
        .await;
    }
    if provider_protocol(config) == ProviderProtocol::AnthropicMessages {
        return run_anthropic_agent_loop(
            paths,
            config,
            first_input,
            toolbox,
            box_id,
            mcp_registry,
            mcp_tools,
            store,
            project_key,
            session_id,
            control,
            observer,
            instructions,
        )
        .await;
    }
    let mut previous_response_id: Option<String> = None;
    let mut input = first_input;
    let stateless = provider_is_stateless(config);
    let mut stateless_input_items = Vec::new();
    if stateless {
        append_response_input_items(&mut stateless_input_items, input.clone());
        input = Value::Array(stateless_input_items.clone());
    }
    let mut last_text = String::new();
    let mut last_tool_summary: Option<String> = None;
    let mut last_plan_fingerprint: Option<String> = None;
    let mut repeated_plan_only_turns = 0usize;
    let mut awaiting_final_answer_after_tools = false;
    let mut last_tool_fingerprint: Option<String> = None;
    let mut repeated_tool_turns = 0usize;
    let mut grounding_repair_sent = false;
    let mut empty_tool_continuations = 0usize;
    let mut parser_recoveries = 0usize;
    let mut provider_request_recoveries = 0usize;

    for _ in 0..MAX_AGENT_BACKEND_TURNS {
        control.check()?;
        let request_input = if stateless {
            Value::Array(stateless_input_items.clone())
        } else {
            input.clone()
        };
        store.append_command(
            project_key,
            session_id,
            &["responses.create".to_string(), config.model.clone()],
            "ok",
            Some(0),
            &format!("instructions=developer tools={}", BOX_TOOL_NAMES.join(",")),
            "",
        )?;
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_request",
            json!({
                "backend": "responses",
                "model": config.model.clone(),
                "stateless": stateless,
                "previous_response_id": previous_response_id.clone(),
                "tools": BOX_TOOL_NAMES,
            }),
        )?;
        observer.on_event(AgentEvent::Status("sending request to responses backend"));
        let mut body = json!({
            "model": config.model,
            "instructions": instructions,
            "input": request_input,
            "stream": true,
            "store": !stateless,
            "previous_response_id": if stateless { None } else { previous_response_id.clone() },
            "metadata": {
                "client": "wirecli",
                "project_key": project_key,
                "session_id": session_id,
                "box_id": box_id
            }
        });
        body["tools"] = tools.clone();
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(provider.supports_parallel_tool_calls);

        let response = match client
            .post(format!("{}/responses", upstream_url.trim_end_matches('/')))
            .header(ACCEPT, "text/event-stream")
            .header(ACCEPT_ENCODING, "identity")
            .headers(provider_headers(config)?)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let message = format!("responses request failed before stream: {err}");
                match provider_request_error_after_tools_action(
                    "responses",
                    "responses endpoint",
                    &message,
                    awaiting_final_answer_after_tools,
                    last_tool_summary.as_deref(),
                    &mut provider_request_recoveries,
                    store,
                    project_key,
                    session_id,
                    observer,
                ) {
                    ProviderRequestErrorAction::Retry => {
                        tokio::time::sleep(Duration::from_millis(PROVIDER_REQUEST_RETRY_DELAY_MS))
                            .await;
                        continue;
                    }
                    ProviderRequestErrorAction::Finish(text) => return Ok(text),
                    ProviderRequestErrorAction::None => {}
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "responses",
                        "stage": "request",
                        "error": message.clone(),
                    }),
                );
                return Err(message);
            }
        };
        control.check()?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = upstream_status_error("responses endpoint", status, &text);
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_error",
                json!({
                    "backend": "responses",
                    "stage": "status",
                    "status": status.as_u16(),
                    "error": message.clone(),
                }),
            );
            return Err(message);
        }

        let turn_result = {
            let mut stream_recorder =
                StreamCheckpointRecorder::new(store, project_key, session_id, "responses");
            let result = consume_stream(response, observer, control, &mut stream_recorder).await;
            if result.is_ok() {
                stream_recorder.flush("stream_completed");
            } else {
                stream_recorder.flush("stream_error_checkpoint");
            }
            result
        };
        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                if parser_recoveries < MAX_WDF_PARSER_RECOVERIES && recoverable_parser_error(&err) {
                    parser_recoveries += 1;
                    observer.on_event(AgentEvent::Status(
                        "ACP/WDF stopped malformed responses stream and requested a clean continuation",
                    ));
                    let repair_prompt = acp::wdf_parser_error_prompt("responses", &err);
                    let assessment = acp::AcpAssessment::wdf(
                        "responses",
                        if awaiting_final_answer_after_tools {
                            AcpPhase::AwaitingAssistantAfterTools
                        } else {
                            AcpPhase::AwaitingAssistant
                        },
                        acp::AcpViolation::ParserStreamError,
                        "parser stopped malformed stream and dropped the request",
                        repair_prompt.clone(),
                    );
                    append_acp_wdf_checkpoint(
                        store,
                        project_key,
                        session_id,
                        &assessment,
                        parser_recoveries,
                    );
                    if stateless {
                        append_response_input_items(
                            &mut stateless_input_items,
                            json!(repair_prompt),
                        );
                        input = Value::Array(stateless_input_items.clone());
                    } else {
                        input = json!(repair_prompt);
                    }
                    continue;
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "responses",
                        "stage": "stream",
                        "error": err.clone(),
                    }),
                );
                return Err(err);
            }
        };
        provider_request_recoveries = 0;
        let turn_response_id = turn.response_id.clone();
        previous_response_id = turn_response_id.clone();
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_turn",
            json!({
                "backend": "responses",
                "response_id": turn_response_id,
                "text_chars": turn.text.chars().count(),
                "tool_calls": turn.tool_calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
                "usage": turn.usage.clone(),
            }),
        )?;
        if let Some(usage) = turn.usage.clone() {
            observer.on_event(AgentEvent::Usage(usage));
        }

        if !turn.tool_calls.is_empty() {
            let mut outputs = Vec::new();
            let mut turn_tool_names = Vec::new();
            let mut turn_tool_outputs = Vec::new();
            let completed_calls = execute_tool_calls(
                paths,
                config,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                mcp_registry,
                mcp_tools,
                turn.tool_calls,
                control,
                observer,
            )?;
            for completed in &completed_calls {
                let call = &completed.call;
                let result = &completed.result;
                if stateless {
                    stateless_input_items.push(function_call_input_item(call));
                }
                turn_tool_names.push(call.name.clone());
                turn_tool_outputs.push(result.model_output.clone());
                let output_item = json!({
                    "type": "function_call_output",
                    "call_id": call.call_id.clone(),
                    "name": call.name.clone(),
                    "output": result.model_output.clone(),
                });
                if stateless {
                    stateless_input_items.push(output_item.clone());
                }
                outputs.push(output_item);
            }
            last_tool_summary = Some(summarize_completed_tool_turn(&completed_calls));
            let fingerprint = tool_turn_fingerprint(&turn_tool_names, &turn_tool_outputs);
            if last_tool_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                repeated_tool_turns = repeated_tool_turns.saturating_add(1);
            } else {
                repeated_tool_turns = 1;
                last_tool_fingerprint = Some(fingerprint);
            }
            if repeated_tool_turns >= MAX_REPEATED_IDENTICAL_TOOL_TURNS {
                let summary = last_tool_summary.clone().unwrap_or_default();
                if tool_outputs_need_repair(&turn_tool_outputs) {
                    observer.on_event(AgentEvent::Status(
                        "repeated recoverable tool error; asking model to choose another route",
                    ));
                    append_agent_checkpoint_best_effort(
                        store,
                        project_key,
                        session_id,
                        "tool_repair_prompt",
                        json!({
                            "backend": "responses",
                            "reason": "repeated_recoverable_tool_error",
                            "summary": summary.clone(),
                        }),
                    );
                    let repair_prompt = repeated_tool_repair_prompt(&summary);
                    if stateless {
                        append_response_input_items(
                            &mut stateless_input_items,
                            json!(repair_prompt),
                        );
                        input = Value::Array(stateless_input_items.clone());
                    } else {
                        append_response_input_items(&mut outputs, json!(repair_prompt));
                        input = Value::Array(outputs);
                    }
                    repeated_tool_turns = 0;
                    last_tool_fingerprint = None;
                    empty_tool_continuations = 0;
                    awaiting_final_answer_after_tools = true;
                    continue;
                }
                last_text = format!(
                    "Loop guard stopped repeated identical tool execution.\n\n{}",
                    summary
                );
                break;
            }
            awaiting_final_answer_after_tools = true;
            if turn_tool_names.iter().all(|name| is_plan_tool(name)) {
                let fingerprint = turn_tool_outputs.join("\n---\n");
                if last_plan_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    repeated_plan_only_turns = repeated_plan_only_turns.saturating_add(1);
                } else {
                    repeated_plan_only_turns = 1;
                    last_plan_fingerprint = Some(fingerprint);
                }
                if repeated_plan_only_turns >= 2
                    && turn_tool_outputs
                        .iter()
                        .any(|output| plan_output_is_complete(output))
                {
                    last_text = "Provider repeated a completed plan without final text. The plan is complete; use a follow-up prompt if you want the final report regenerated.".to_string();
                    break;
                }
            } else {
                repeated_plan_only_turns = 0;
                last_plan_fingerprint = None;
            }
            observer.on_event(AgentEvent::Status(
                "sending tool results to responses backend",
            ));
            input = if stateless {
                Value::Array(stateless_input_items.clone())
            } else {
                Value::Array(outputs)
            };
            empty_tool_continuations = 0;
            continue;
        }

        if !turn.text.trim().is_empty() {
            if let Some(repair_prompt) =
                grounding_repair_prompt(paths, &turn.text, last_tool_summary.as_deref())
            {
                if grounding_repair_sent {
                    last_text = grounding_blocked_text(&grounding_violations(paths, &turn.text));
                    break;
                }
                observer.on_event(AgentEvent::Status(
                    "grounding check rejected unverified final answer",
                ));
                if stateless {
                    append_response_input_items(&mut stateless_input_items, json!(repair_prompt));
                    input = Value::Array(stateless_input_items.clone());
                } else {
                    input = json!(repair_prompt);
                }
                grounding_repair_sent = true;
                continue;
            }
            last_text = turn.text;
            break;
        }

        if awaiting_final_answer_after_tools {
            let summary = last_tool_summary
                .clone()
                .unwrap_or_else(|| "No tool summary available.".to_string());
            if empty_tool_continuations < MAX_EMPTY_TOOL_CONTINUATIONS {
                empty_tool_continuations += 1;
                observer.on_event(AgentEvent::Status(
                    "ACP/WDF recovered empty assistant turn after tool results",
                ));
                let assessment = acp::assess_assistant_turn(
                    "responses",
                    AcpPhase::AwaitingAssistantAfterTools,
                    "",
                    0,
                    Some(&summary),
                );
                append_acp_wdf_checkpoint(
                    store,
                    project_key,
                    session_id,
                    &assessment,
                    empty_tool_continuations,
                );
                let continuation = assessment
                    .repair_prompt
                    .clone()
                    .unwrap_or_else(|| tool_continuation_prompt(&summary));
                if stateless {
                    append_response_input_items(&mut stateless_input_items, json!(continuation));
                    input = Value::Array(stateless_input_items.clone());
                } else {
                    input = json!(continuation);
                }
                continue;
            }
            observer.on_event(AgentEvent::Status(
                "stopped after empty provider turn without final text",
            ));
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_empty_stop",
                json!({
                        "backend": "responses",
                    "summary": summary.clone(),
                }),
            );
            last_text = tool_checkpoint_after_empty_provider_text(&summary);
            break;
        }

        observer.on_event(AgentEvent::Status(
            "responses completed without visible output",
        ));
        if let Some(summary) = last_tool_summary {
            return Err(empty_stream_after_tools_error(
                "responses endpoint",
                &summary,
            ));
        }
        return Err(empty_stream_error("responses endpoint"));
    }

    if last_text.is_empty() {
        if let Some(summary) = last_tool_summary {
            return Ok(tool_checkpoint_after_empty_provider_text(&summary));
        }
        return Err("agent completed without a final text response".to_string());
    }

    Ok(last_text)
}

async fn run_chat_agent_loop(
    paths: &AppPaths,
    config: &AppConfig,
    first_input: Value,
    toolbox: &BoxTools<'_>,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    control: &AgentControl,
    observer: &mut dyn AgentObserver,
    instructions: String,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let provider = active_provider(config);
    let tools = chat_tool_definitions(paths, mcp_tools);
    let mut messages = vec![
        json!({ "role": "system", "content": instructions }),
        chat_user_message(first_input),
    ];
    let mut last_text = String::new();
    let mut last_tool_summary: Option<String> = None;
    let mut last_plan_fingerprint: Option<String> = None;
    let mut repeated_plan_only_turns = 0usize;
    let mut awaiting_final_answer_after_tools = false;
    let mut last_tool_fingerprint: Option<String> = None;
    let mut repeated_tool_turns = 0usize;
    let mut grounding_repair_sent = false;
    let mut empty_tool_continuations = 0usize;
    let mut parser_recoveries = 0usize;
    let mut provider_request_recoveries = 0usize;

    for _ in 0..MAX_AGENT_BACKEND_TURNS {
        control.check()?;
        store.append_command(
            project_key,
            session_id,
            &["chat.completions.create".to_string(), config.model.clone()],
            "ok",
            Some(0),
            &format!(
                "messages={} tools={}",
                messages.len(),
                BOX_TOOL_NAMES.join(",")
            ),
            "",
        )?;
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_request",
            json!({
                "backend": "chat_completions",
                "model": config.model.clone(),
                "messages": messages.len(),
                "tools": BOX_TOOL_NAMES,
            }),
        )?;
        observer.on_event(AgentEvent::Status(
            "sending request to chat completions backend",
        ));
        let mut body = json!({
            "model": config.model,
            "messages": messages,
            "stream": true,
        });
        body["tools"] = tools.clone();
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(provider.supports_parallel_tool_calls);
        apply_reasoning_effort(&mut body, config);
        if provider.supports_prompt_cache {
            body["prompt_cache_key"] = json!(format!(
                "{}:{}:{}",
                project_key, config.base_url, config.model
            ));
        }

        let response = match client
            .post(format!(
                "{}/chat/completions",
                config.base_url.trim_end_matches('/')
            ))
            .header(ACCEPT, "text/event-stream")
            .header(ACCEPT_ENCODING, "identity")
            .headers(provider_headers(config)?)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let message = format!("chat completions request failed before stream: {err}");
                match provider_request_error_after_tools_action(
                    "chat_completions",
                    "chat completions endpoint",
                    &message,
                    awaiting_final_answer_after_tools,
                    last_tool_summary.as_deref(),
                    &mut provider_request_recoveries,
                    store,
                    project_key,
                    session_id,
                    observer,
                ) {
                    ProviderRequestErrorAction::Retry => {
                        tokio::time::sleep(Duration::from_millis(PROVIDER_REQUEST_RETRY_DELAY_MS))
                            .await;
                        continue;
                    }
                    ProviderRequestErrorAction::Finish(text) => return Ok(text),
                    ProviderRequestErrorAction::None => {}
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "chat_completions",
                        "stage": "request",
                        "error": message.clone(),
                    }),
                );
                return Err(message);
            }
        };
        control.check()?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = upstream_status_error("chat completions endpoint", status, &text);
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_error",
                json!({
                    "backend": "chat_completions",
                    "stage": "status",
                    "status": status.as_u16(),
                    "error": message.clone(),
                }),
            );
            return Err(message);
        }

        let turn_result = {
            let mut stream_recorder =
                StreamCheckpointRecorder::new(store, project_key, session_id, "chat_completions");
            let result =
                consume_chat_stream(response, observer, control, &mut stream_recorder).await;
            if result.is_ok() {
                stream_recorder.flush("stream_completed");
            } else {
                stream_recorder.flush("stream_error_checkpoint");
            }
            result
        };
        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                if parser_recoveries < MAX_WDF_PARSER_RECOVERIES && recoverable_parser_error(&err) {
                    parser_recoveries += 1;
                    observer.on_event(AgentEvent::Status(
                        "ACP/WDF stopped malformed chat stream and requested a clean continuation",
                    ));
                    let repair_prompt = acp::wdf_parser_error_prompt("chat_completions", &err);
                    let assessment = acp::AcpAssessment::wdf(
                        "chat_completions",
                        if awaiting_final_answer_after_tools {
                            AcpPhase::AwaitingAssistantAfterTools
                        } else {
                            AcpPhase::AwaitingAssistant
                        },
                        acp::AcpViolation::ParserStreamError,
                        "parser stopped malformed stream and dropped the request",
                        repair_prompt.clone(),
                    );
                    append_acp_wdf_checkpoint(
                        store,
                        project_key,
                        session_id,
                        &assessment,
                        parser_recoveries,
                    );
                    messages.push(json!({
                        "role": "user",
                        "content": repair_prompt
                    }));
                    continue;
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "chat_completions",
                        "stage": "stream",
                        "error": err.clone(),
                    }),
                );
                return Err(err);
            }
        };
        provider_request_recoveries = 0;
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_turn",
            json!({
                "backend": "chat_completions",
                "text_chars": turn.text.chars().count(),
                "tool_calls": turn.tool_calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
                "usage": turn.usage.clone(),
            }),
        )?;
        if let Some(usage) = turn.usage.clone() {
            observer.on_event(AgentEvent::Usage(usage));
        }

        if !turn.tool_calls.is_empty() {
            let assistant_content = if turn.text.trim().is_empty() {
                Value::Null
            } else {
                json!(turn.text.clone())
            };
            messages.push(json!({
                "role": "assistant",
                "content": assistant_content,
                "tool_calls": chat_assistant_tool_calls(&turn.tool_calls),
            }));

            let completed_calls = execute_tool_calls(
                paths,
                config,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                mcp_registry,
                mcp_tools,
                turn.tool_calls,
                control,
                observer,
            )?;
            let mut turn_tool_names = Vec::new();
            let mut turn_tool_outputs = Vec::new();
            for completed in &completed_calls {
                turn_tool_names.push(completed.call.name.clone());
                turn_tool_outputs.push(completed.result.model_output.clone());
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": completed.call.call_id.clone(),
                    "content": completed.result.model_output.clone(),
                }));
            }

            last_tool_summary = Some(summarize_completed_tool_turn(&completed_calls));
            let fingerprint = tool_turn_fingerprint(&turn_tool_names, &turn_tool_outputs);
            if last_tool_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                repeated_tool_turns = repeated_tool_turns.saturating_add(1);
            } else {
                repeated_tool_turns = 1;
                last_tool_fingerprint = Some(fingerprint);
            }
            if repeated_tool_turns >= MAX_REPEATED_IDENTICAL_TOOL_TURNS {
                let summary = last_tool_summary.clone().unwrap_or_default();
                if tool_outputs_need_repair(&turn_tool_outputs) {
                    observer.on_event(AgentEvent::Status(
                        "repeated recoverable tool error; asking model to choose another route",
                    ));
                    append_agent_checkpoint_best_effort(
                        store,
                        project_key,
                        session_id,
                        "tool_repair_prompt",
                        json!({
                            "backend": "chat_completions",
                            "reason": "repeated_recoverable_tool_error",
                            "summary": summary.clone(),
                        }),
                    );
                    messages.push(json!({
                        "role": "user",
                        "content": repeated_tool_repair_prompt(&summary)
                    }));
                    repeated_tool_turns = 0;
                    last_tool_fingerprint = None;
                    empty_tool_continuations = 0;
                    awaiting_final_answer_after_tools = true;
                    continue;
                }
                last_text = format!(
                    "Loop guard stopped repeated identical tool execution.\n\n{}",
                    summary
                );
                break;
            }
            awaiting_final_answer_after_tools = true;
            if turn_tool_names.iter().all(|name| is_plan_tool(name)) {
                let fingerprint = turn_tool_outputs.join("\n---\n");
                if last_plan_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    repeated_plan_only_turns = repeated_plan_only_turns.saturating_add(1);
                } else {
                    repeated_plan_only_turns = 1;
                    last_plan_fingerprint = Some(fingerprint);
                }
                if repeated_plan_only_turns >= 2
                    && turn_tool_outputs
                        .iter()
                        .any(|output| plan_output_is_complete(output))
                {
                    last_text = "Provider repeated a completed plan without final text. The plan is complete; use a follow-up prompt if you want the final report regenerated.".to_string();
                    break;
                }
            } else {
                repeated_plan_only_turns = 0;
                last_plan_fingerprint = None;
            }
            observer.on_event(AgentEvent::Status(
                "sending tool results to chat completions backend",
            ));
            empty_tool_continuations = 0;
            continue;
        }

        if !turn.text.trim().is_empty() {
            if let Some(repair_prompt) =
                grounding_repair_prompt(paths, &turn.text, last_tool_summary.as_deref())
            {
                if grounding_repair_sent {
                    last_text = grounding_blocked_text(&grounding_violations(paths, &turn.text));
                    break;
                }
                observer.on_event(AgentEvent::Status(
                    "grounding check rejected unverified final answer",
                ));
                messages.push(json!({
                    "role": "user",
                    "content": repair_prompt
                }));
                grounding_repair_sent = true;
                continue;
            }
            last_text = turn.text;
            break;
        }

        if awaiting_final_answer_after_tools {
            let summary = last_tool_summary
                .clone()
                .unwrap_or_else(|| "No tool summary available.".to_string());
            if empty_tool_continuations < MAX_EMPTY_TOOL_CONTINUATIONS {
                empty_tool_continuations += 1;
                observer.on_event(AgentEvent::Status(
                    "ACP/WDF recovered empty assistant turn after tool results",
                ));
                let assessment = acp::assess_assistant_turn(
                    "chat_completions",
                    AcpPhase::AwaitingAssistantAfterTools,
                    "",
                    0,
                    Some(&summary),
                );
                append_acp_wdf_checkpoint(
                    store,
                    project_key,
                    session_id,
                    &assessment,
                    empty_tool_continuations,
                );
                let continuation = assessment
                    .repair_prompt
                    .clone()
                    .unwrap_or_else(|| tool_continuation_prompt(&summary));
                messages.push(json!({
                    "role": "user",
                    "content": continuation
                }));
                continue;
            }
            observer.on_event(AgentEvent::Status(
                "stopped after empty provider turn without final text",
            ));
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_empty_stop",
                json!({
                    "backend": "chat_completions",
                    "summary": summary.clone(),
                }),
            );
            last_text = tool_checkpoint_after_empty_provider_text(&summary);
            break;
        }

        observer.on_event(AgentEvent::Status(
            "chat completions completed without visible output",
        ));
        if let Some(summary) = last_tool_summary {
            return Err(empty_stream_after_tools_error(
                "chat completions endpoint",
                &summary,
            ));
        }
        return Err(empty_stream_error("chat completions endpoint"));
    }

    if last_text.is_empty() {
        if let Some(summary) = last_tool_summary {
            return Ok(tool_checkpoint_after_empty_provider_text(&summary));
        }
        return Err("agent completed without a final text response".to_string());
    }

    Ok(last_text)
}

fn is_plan_tool(name: &str) -> bool {
    matches!(name, "plan" | "update_plan")
}

fn append_response_input_items(items: &mut Vec<Value>, value: Value) {
    match value {
        Value::Array(values) => items.extend(values),
        Value::String(text) => items.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }]
        })),
        other => items.push(other),
    }
}

fn function_call_input_item(call: &ToolCall) -> Value {
    let arguments = match &call.arguments {
        Value::String(raw) => raw.clone(),
        value => value.to_string(),
    };
    json!({
        "type": "function_call",
        "id": call.call_id.clone(),
        "call_id": call.call_id.clone(),
        "name": call.name.clone(),
        "arguments": arguments
    })
}

fn summarize_completed_tool_turn(completed: &[CompletedToolCall]) -> String {
    let mut lines = Vec::new();
    lines.push("Last completed tools checkpoint:".to_string());
    for completed in completed {
        let name = completed.call.name.as_str();
        let summary = completed.summary.trim();
        if summary.is_empty() {
            lines.push(format!("- {name}"));
        } else {
            lines.push(format!("- {name}: {summary}"));
        }
        let evidence = tool_checkpoint_excerpt(&completed.result.model_output);
        if !evidence.trim().is_empty() {
            lines.push("  evidence:".to_string());
            lines.push("  ```text".to_string());
            for line in evidence.lines() {
                lines.push(format!("  {line}"));
            }
            lines.push("  ```".to_string());
        }
    }
    lines.join("\n")
}

fn tool_continuation_prompt(summary: &str) -> String {
    acp::wdf_tool_continuation_prompt(summary)
}

enum ProviderRequestErrorAction {
    Retry,
    Finish(String),
    None,
}

const STREAM_CHECKPOINT_TEXT_STEP: usize = 192;
const STREAM_CHECKPOINT_TOOL_STEP: usize = 96;

#[derive(Default)]
struct StreamToolCheckpoint {
    name: String,
    arguments: String,
}

trait StreamCheckpointSink {
    fn record_text_delta(&mut self, delta: &str);
    fn record_tool_delta(
        &mut self,
        call_id: Option<&str>,
        name: Option<&str>,
        arguments_delta: &str,
    );
}

struct NoopStreamCheckpointSink;

impl StreamCheckpointSink for NoopStreamCheckpointSink {
    fn record_text_delta(&mut self, _delta: &str) {}

    fn record_tool_delta(
        &mut self,
        _call_id: Option<&str>,
        _name: Option<&str>,
        _arguments_delta: &str,
    ) {
    }
}

struct StreamCheckpointRecorder<'a> {
    store: &'a mut SessionStore,
    project_key: &'a str,
    session_id: &'a str,
    backend: &'a str,
    text: String,
    tools: HashMap<String, StreamToolCheckpoint>,
    last_text_chars: usize,
    last_tool_chars: usize,
    last_tool_count: usize,
    snapshots: usize,
}

impl<'a> StreamCheckpointRecorder<'a> {
    fn new(
        store: &'a mut SessionStore,
        project_key: &'a str,
        session_id: &'a str,
        backend: &'a str,
    ) -> Self {
        Self {
            store,
            project_key,
            session_id,
            backend,
            text: String::new(),
            tools: HashMap::new(),
            last_text_chars: 0,
            last_tool_chars: 0,
            last_tool_count: 0,
            snapshots: 0,
        }
    }

    fn record_text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        self.flush_if_needed("stream_partial");
    }

    fn record_tool_delta(
        &mut self,
        call_id: Option<&str>,
        name: Option<&str>,
        arguments_delta: &str,
    ) {
        let key = call_id
            .filter(|value| !value.trim().is_empty())
            .or(name)
            .unwrap_or("pending_tool_call")
            .to_string();
        let tool = self.tools.entry(key).or_default();
        if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
            tool.name = name.to_string();
        }
        if !arguments_delta.is_empty() {
            tool.arguments.push_str(arguments_delta);
        }
        self.flush_if_needed("tool_call_partial");
    }

    fn flush_if_needed(&mut self, phase: &str) {
        let text_chars = self.text.chars().count();
        let tool_chars = self.tool_chars();
        let tool_count = self.tools.len();
        if (self.snapshots == 0 && (text_chars > 0 || tool_count > 0))
            || text_chars.saturating_sub(self.last_text_chars) >= STREAM_CHECKPOINT_TEXT_STEP
            || tool_chars.saturating_sub(self.last_tool_chars) >= STREAM_CHECKPOINT_TOOL_STEP
            || tool_count > self.last_tool_count
        {
            self.flush(phase);
        }
    }

    fn flush(&mut self, phase: &str) {
        if self.text.trim().is_empty() && self.tools.is_empty() {
            return;
        }
        self.snapshots = self.snapshots.saturating_add(1);
        let text_chars = self.text.chars().count();
        let tool_chars = self.tool_chars();
        let tools = self
            .tools
            .iter()
            .map(|(call_id, tool)| {
                json!({
                    "call_id": call_id,
                    "name": tool.name,
                    "arguments_chars": tool.arguments.chars().count(),
                    "arguments_excerpt": truncate_inline(&tool.arguments, 1600),
                })
            })
            .collect::<Vec<_>>();
        append_agent_checkpoint_best_effort(
            self.store,
            self.project_key,
            self.session_id,
            phase,
            json!({
                "backend": self.backend,
                "snapshot_index": self.snapshots,
                "text_chars": text_chars,
                "text_excerpt": truncate_inline(&self.text, 2400),
                "tools": tools,
            }),
        );
        let _ = self.store.replace_session_memory(
            self.project_key,
            self.session_id,
            "agent_state",
            &self.render_memory_text(phase, text_chars, tool_chars),
            &[
                "agent_state".to_string(),
                "stream".to_string(),
                self.backend.to_string(),
                phase.to_string(),
            ],
        );
        self.last_text_chars = text_chars;
        self.last_tool_chars = tool_chars;
        self.last_tool_count = self.tools.len();
    }

    fn tool_chars(&self) -> usize {
        self.tools
            .values()
            .map(|tool| tool.arguments.chars().count())
            .sum()
    }

    fn render_memory_text(&self, phase: &str, text_chars: usize, tool_chars: usize) -> String {
        let mut out = format!(
            "Latest agent execution state: backend={} phase={} snapshot={} text_chars={} tool_argument_chars={}.",
            self.backend, phase, self.snapshots, text_chars, tool_chars
        );
        if !self.text.trim().is_empty() {
            out.push_str("\nPartial assistant text:\n");
            out.push_str(&truncate_inline(&self.text, 1800));
        }
        if !self.tools.is_empty() {
            out.push_str("\nPartial tool calls:");
            for (call_id, tool) in &self.tools {
                out.push_str("\n- ");
                if tool.name.is_empty() {
                    out.push_str("pending_tool");
                } else {
                    out.push_str(&tool.name);
                }
                out.push_str(" call_id=");
                out.push_str(call_id);
                out.push_str(" args=");
                out.push_str(&truncate_inline(&tool.arguments, 900));
            }
        }
        out
    }
}

impl StreamCheckpointSink for StreamCheckpointRecorder<'_> {
    fn record_text_delta(&mut self, delta: &str) {
        StreamCheckpointRecorder::record_text_delta(self, delta);
    }

    fn record_tool_delta(
        &mut self,
        call_id: Option<&str>,
        name: Option<&str>,
        arguments_delta: &str,
    ) {
        StreamCheckpointRecorder::record_tool_delta(self, call_id, name, arguments_delta);
    }
}

fn provider_request_error_after_tools_action(
    backend: &str,
    endpoint: &str,
    message: &str,
    awaiting_final_answer_after_tools: bool,
    last_tool_summary: Option<&str>,
    provider_request_recoveries: &mut usize,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    observer: &mut dyn AgentObserver,
) -> ProviderRequestErrorAction {
    if !awaiting_final_answer_after_tools {
        return ProviderRequestErrorAction::None;
    }
    let Some(summary) = last_tool_summary else {
        return ProviderRequestErrorAction::None;
    };

    if *provider_request_recoveries < MAX_PROVIDER_REQUEST_RETRIES_AFTER_TOOLS {
        *provider_request_recoveries += 1;
        observer.on_event(AgentEvent::Status(
            "provider transport failed after tool results; retrying from checkpoint",
        ));
        append_agent_checkpoint_best_effort(
            store,
            project_key,
            session_id,
            "provider_transport_retry",
            json!({
                "backend": backend,
                "stage": "request_after_tools",
                "continuation": *provider_request_recoveries,
                "error": redact_secrets(message),
                "summary": summary,
            }),
        );
        return ProviderRequestErrorAction::Retry;
    }

    observer.on_event(AgentEvent::Status(
        "provider transport failed after tool results; saved continuation checkpoint",
    ));
    append_agent_checkpoint_best_effort(
        store,
        project_key,
        session_id,
        "provider_transport_checkpoint",
        json!({
            "backend": backend,
            "stage": "request_after_tools",
            "error": redact_secrets(message),
            "summary": summary,
        }),
    );
    ProviderRequestErrorAction::Finish(tool_checkpoint_after_provider_request_error(
        endpoint, message, summary,
    ))
}

fn append_acp_wdf_checkpoint(
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    assessment: &acp::AcpAssessment,
    continuation: usize,
) {
    append_agent_checkpoint_best_effort(
        store,
        project_key,
        session_id,
        "acp_wdf_repair",
        json!({
            "backend": assessment.backend.clone(),
            "phase": assessment.phase,
            "status": assessment.status.clone(),
            "violation": assessment.violation.clone(),
            "continuation": continuation,
            "note": assessment.note.clone(),
        }),
    );
}

fn recoverable_parser_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("invalid json event")
        || lower.contains("invalid trailing json")
        || lower.contains("non-utf8 sse bytes")
        || lower.contains("malformed")
        || lower.contains("parser")
}

fn tool_outputs_need_repair(outputs: &[String]) -> bool {
    outputs.iter().any(|output| {
        let lower = output.to_ascii_lowercase();
        lower.contains("tool error in `")
            || lower.contains("lattice blocked `")
            || lower.contains("watchdog stopped `")
            || lower.contains("command approval required")
            || lower.contains("approval denied")
            || lower.contains("status: blocked")
            || lower.contains("verifier pipeline\nstatus: failed")
    })
}

fn repeated_tool_repair_prompt(summary: &str) -> String {
    format!(
        "The previous tool call failed or was blocked, and repeating the identical call will not make progress. Continue the same task without stopping: inspect the error, choose a different allowed tool or safer command, narrow the arguments, request approval only when the command itself requires it, or explain the blocker if no safe route exists. Do not retry the same failing tool call unchanged.\n\n{summary}"
    )
}

fn tool_checkpoint_after_empty_provider_text(summary: &str) -> String {
    format!(
        "Provider returned empty tool follow-ups after Wire CLI continued from the latest tool checkpoint. Wire CLI saved the checkpoint instead of inventing progress.\n\n{summary}\n\nContinue this same session to resume from the checkpoint."
    )
}

fn tool_checkpoint_after_provider_request_error(
    endpoint: &str,
    err: &str,
    summary: &str,
) -> String {
    format!(
        "Provider transport failed after Wire CLI saved the latest tool checkpoint. Wire CLI did not discard the work already completed by tools.\n\nEndpoint: {endpoint}\nDetails: {}\n\n{summary}\n\nFix or restart the provider if needed, then continue this same session to resume from the checkpoint.",
        redact_secrets(err)
    )
}

fn tool_checkpoint_excerpt(output: &str) -> String {
    const MAX_CHARS: usize = 2400;
    let output = redact_secrets(output.trim());
    if output.chars().count() <= MAX_CHARS {
        return output;
    }
    let lines = output.lines().collect::<Vec<_>>();
    let head = lines.iter().take(24).copied().collect::<Vec<_>>();
    let tail = lines.iter().rev().take(12).copied().collect::<Vec<_>>();
    let mut out = String::new();
    out.push_str(&head.join("\n"));
    out.push_str("\n...\n");
    for line in tail.into_iter().rev() {
        out.push_str(line);
        out.push('\n');
    }
    truncate_inline(out.trim(), MAX_CHARS)
}

fn provider_error_session_text(err: &str) -> String {
    format!(
        "Wire CLI provider error.\n\
         Details: {}\n\n\
         Wire CLI did not send a recovery prompt. Fix the provider/model/account issue, then continue the same session.",
        redact_secrets(err)
    )
}

fn tool_turn_fingerprint(names: &[String], outputs: &[String]) -> String {
    let mut text = String::new();
    for (name, output) in names.iter().zip(outputs.iter()) {
        text.push_str(name);
        text.push(':');
        text.push_str(&truncate_inline(output, 600));
        text.push('\n');
    }
    text
}

fn grounding_repair_prompt(
    paths: &AppPaths,
    text: &str,
    last_tool_summary: Option<&str>,
) -> Option<String> {
    let violations = grounding_violations(paths, text);
    if violations.is_empty() {
        return None;
    }

    let mut out = String::from(
        "Grounding check rejected the previous answer. Rewrite it from verified repository evidence only.\n",
    );
    out.push_str("Problems detected:\n");
    for violation in &violations {
        out.push_str("- ");
        out.push_str(violation);
        out.push('\n');
    }
    out.push_str("\nRules for the corrected answer:\n");
    out.push_str("- Use only real tool names from this closed set: ");
    out.push_str(&BOX_TOOL_NAMES.join(", "));
    out.push_str(".\n");
    out.push_str("- Do not mention placeholder crate names, sample paths, framework files, or unavailable tool aliases.\n");
    out.push_str("- Treat Lattice only as the Box execution/path boundary.\n");
    out.push_str(
        "- If the available evidence is not enough, call inspection tools instead of answering.\n",
    );
    if let Some(summary) = last_tool_summary.filter(|summary| !summary.trim().is_empty()) {
        out.push_str("\nLast tool evidence:\n");
        out.push_str(summary.trim());
        out.push('\n');
    }
    out.push_str("\nRejected answer excerpt:\n");
    out.push_str(&truncate_inline(text, 1200));
    Some(out)
}

fn grounding_violations(paths: &AppPaths, text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut violations = Vec::new();

    if mentions_unavailable_tool(&lower, "add_file") {
        violations.push(
            "mentioned unavailable tool `add_file`; use `write_file` or `apply_patch`".to_string(),
        );
    }
    if mentions_unavailable_tool(&lower, "file_read") {
        violations.push("mentioned unavailable tool `file_read`; use `read_file`".to_string());
    }
    if mentions_unavailable_tool(&lower, "file_write") {
        violations.push(
            "mentioned unavailable tool `file_write`; use `write_file` or `apply_patch`"
                .to_string(),
        );
    }
    if mentions_standalone_memory_tool(&lower) {
        violations.push("mentioned a standalone `memory` tool; use `remember`, `recall`, `session_remember`, `session_recall`, `lab_learn`, or `lab_recall`".to_string());
    }
    if lower.contains("rifice") {
        violations
            .push("mentioned `rifice`, which looks like a placeholder crate/path name".to_string());
    }
    if lower.contains("could be your crate name")
        || lower.contains("poderia ser o nome do crate")
        || lower.contains("pode ser o nome do crate")
    {
        violations.push(
            "used a placeholder crate-name explanation instead of repository evidence".to_string(),
        );
    }
    if lower.contains("prisma/schema.prisma")
        && !paths.root_dir.join("prisma/schema.prisma").exists()
    {
        violations
            .push("mentioned `prisma/schema.prisma`, but that path is not present".to_string());
    }
    if lower.contains("arquitetura baseada em lattice")
        || lower.contains("arquitetura baseado em lattice")
        || lower.contains("validacao de arquitetura baseada em lattice")
        || lower.contains("validação de arquitetura baseada em lattice")
        || lower.contains("architecture based on lattice")
    {
        violations.push(
            "described Lattice as architecture validation instead of the Box boundary".to_string(),
        );
    }

    violations
}

fn grounding_blocked_text(violations: &[String]) -> String {
    let mut out = String::from(
        "Grounding check blocked the final answer because it still contained unverified claims.\n",
    );
    if !violations.is_empty() {
        out.push_str("\nRemaining problems:\n");
        for violation in violations {
            out.push_str("- ");
            out.push_str(violation);
            out.push('\n');
        }
    }
    out.push_str("\nInspect the repository with real tools before answering.");
    out
}

fn mentions_unavailable_tool(lower_text: &str, tool_name: &str) -> bool {
    if corrective_unavailable_tool_mention(lower_text, tool_name) {
        return false;
    }
    if lower_text.contains(&format!("would use `{tool_name}`"))
        || lower_text.contains(&format!("use `{tool_name}`"))
        || lower_text.contains(&format!("usar `{tool_name}`"))
        || lower_text.contains(&format!("using `{tool_name}`"))
    {
        return true;
    }
    lower_text.lines().any(|line| {
        let line = trim_list_marker(line);
        starts_with_word(line, tool_name)
            && (line.contains(" para ")
                || line.contains(" to ")
                || line.contains("tool")
                || line.contains("access")
                || line.contains("acessar"))
    })
}

fn corrective_unavailable_tool_mention(lower_text: &str, tool_name: &str) -> bool {
    let patterns = [
        format!("`{tool_name}` does not exist"),
        format!("`{tool_name}` doesn't exist"),
        format!("`{tool_name}` is not available"),
        format!("`{tool_name}` is unavailable"),
        format!("`{tool_name}` nao existe"),
        format!("`{tool_name}` não existe"),
        format!("`{tool_name}` nao e uma tool"),
        format!("`{tool_name}` não é uma tool"),
        format!("tool `{tool_name}` does not exist"),
        format!("tool `{tool_name}` is not available"),
    ];
    patterns.iter().any(|pattern| lower_text.contains(pattern))
}

fn mentions_standalone_memory_tool(lower_text: &str) -> bool {
    if lower_text.contains("`memory`") || lower_text.contains("memory tool") {
        return true;
    }
    lower_text.lines().any(|line| {
        let line = trim_list_marker(line);
        starts_with_word(line, "memory")
            && (line.contains(" para ")
                || line.contains(" to ")
                || line.contains("tool")
                || line.contains("armazenar"))
    })
}

fn trim_list_marker(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches(|ch: char| {
            ch.is_ascii_digit()
                || ch == '.'
                || ch == ')'
                || ch == '-'
                || ch == '*'
                || ch == ':'
                || ch == '\u{2022}'
                || ch == '\u{00b7}'
                || ch.is_whitespace()
        })
        .trim_start()
}

fn starts_with_word(text: &str, word: &str) -> bool {
    text.starts_with(word) && word_boundary_after(text, word.len())
}

fn word_boundary_after(text: &str, index: usize) -> bool {
    if index >= text.len() {
        return true;
    }
    text.as_bytes()
        .get(index)
        .map(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
        .unwrap_or(true)
}

fn plan_output_is_complete(output: &str) -> bool {
    let mut has_step = false;
    for line in output.lines() {
        let Some(status) = extract_plan_status(line) else {
            continue;
        };
        has_step = true;
        if !status_is_completed(status) {
            return false;
        }
    }
    has_step
}

fn extract_plan_status(line: &str) -> Option<&str> {
    let line = line
        .trim()
        .trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '.' || ch.is_whitespace())
        .trim_start_matches("- ")
        .trim();
    let rest = line.strip_prefix('[')?;
    rest.split_once(']').map(|(status, _)| status.trim())
}

fn status_is_completed(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed" | "complete" | "done"
    )
}

fn plan_model_output(ui_output: &str, items: &[(String, String)]) -> String {
    if !items.is_empty()
        && items
            .iter()
            .all(|(_, status)| status_is_completed(status.as_str()))
    {
        format!(
            "{ui_output}\nAll plan steps are completed. Do not call plan again; produce the final answer now."
        )
    } else {
        ui_output.to_string()
    }
}

fn recoverable_tool_error(tool_name: &str, error: &str) -> ToolDispatchResult {
    let text = if error.contains("tool watchdog timed out") {
        format!(
            "Watchdog stopped `{tool_name}` after a long stall.\n{error}\nPick a narrower command, use line-focused tools, or switch to a different inspection path before retrying."
        )
    } else if is_lattice_error(error) {
        format!(
            "Lattice blocked `{tool_name}`\n{error}\nUse a direct project-local tool or argv command instead of retrying the same call. For inspection prefer `search`, `grep_lines`, `read_file`, `read_lines`, `glob_files`, or a direct command like `[\"rg\",\"-n\",\"pattern\",\"src\"]`. Absolute `/workspace/...` paths and project paths are allowed when they stay inside the Box."
        )
    } else {
        format!(
            "Tool error in `{tool_name}`\n{error}\nCorrect the tool arguments or choose another tool, then continue."
        )
    };
    ToolDispatchResult {
        model_output: text.clone(),
        ui_output: text,
    }
}

fn maybe_store_language_preference(
    paths: &AppPaths,
    config: &AppConfig,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    user_prompt: &str,
) {
    if !config.memories_enabled() {
        return;
    }
    let Some(note) = crate::context::language_preference_note(user_prompt) else {
        return;
    };
    let tags = vec!["language".to_string(), "preference".to_string()];
    let _ = store.remember_session_memory(project_key, session_id, "preference", &note, &tags);
    if let Ok(memory_context) = MemoryContextStore::new(paths) {
        let _ = memory_context.remember(
            project_key,
            "preference",
            &note,
            &tags,
            Some(session_id),
            "session",
            0.8,
        );
        let _ = memory_context.remember(
            crate::context::WIRE_GLOBAL_MEMORY_KEY,
            "preference",
            &note,
            &tags,
            Some(session_id),
            "global-session",
            0.82,
        );
    }
}

fn maybe_observe_lab(
    paths: &AppPaths,
    config: &AppConfig,
    project_key: &str,
    session_id: &str,
    user_prompt: &str,
) {
    if !config.afup_enabled() {
        return;
    }
    if let Ok(lab) = LabStore::new(paths) {
        let _ = lab.observe_user_prompt(project_key, session_id, user_prompt);
        let _ = lab.observe_user_prompt(
            crate::context::WIRE_GLOBAL_MEMORY_KEY,
            session_id,
            user_prompt,
        );
    }
}

fn maybe_offer_memory_or_skill_suggestion(
    config: &AppConfig,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    user_prompt: &str,
) {
    if !config.memories_enabled() {
        return;
    }
    if let Some(suggestion) = durable_memory_suggestion(user_prompt) {
        let _ = store.append_command(
            project_key,
            session_id,
            &["memory.suggestion".to_string()],
            "pending",
            None,
            &format!(
                "Aprendi isso, quer salvar?\n{}\n\nPara salvar de forma duravel, use `remember` com kind/tags claros. Para manter so nesta sessao, use `session_remember`.",
                suggestion
            ),
            "",
        );
    }
    if let Some(suggestion) = skill_generation_suggestion(user_prompt) {
        let _ = store.append_command(
            project_key,
            session_id,
            &["skill.suggestion".to_string()],
            "pending",
            None,
            &format!(
                "Workflow repetido detectado, quer transformar em skill?\n{}\n\nUse `skill_create` quando o fluxo estiver confirmado.",
                suggestion
            ),
            "",
        );
    }
}

fn durable_memory_suggestion(user_prompt: &str) -> Option<String> {
    let lower = user_prompt.to_ascii_lowercase();
    let triggers = [
        "prefiro ",
        "preferencia",
        "sempre ",
        "nunca ",
        "lembre",
        "memorize",
        "guarde",
        "quando eu pedir",
    ];
    if !triggers.iter().any(|trigger| lower.contains(trigger)) {
        return None;
    }
    Some(redact_secrets(&preview_text(user_prompt, 420)))
}

fn skill_generation_suggestion(user_prompt: &str) -> Option<String> {
    let lower = user_prompt.to_ascii_lowercase();
    let triggers = [
        "workflow repetido",
        "fluxo repetido",
        "toda vez que",
        "sempre que",
        "crie uma skill",
        "criar uma skill",
        "automatiza esse fluxo",
    ];
    if !triggers.iter().any(|trigger| lower.contains(trigger)) {
        return None;
    }
    Some(redact_secrets(&preview_text(user_prompt, 420)))
}

fn is_lattice_error(error: &str) -> bool {
    error.contains("path escapes the box workspace")
        || error.contains("absolute paths are blocked outside the box workspace")
        || error.contains("absolute paths outside the Box are blocked")
        || error.contains("direct shell entrypoints are blocked by Lattice")
        || error.contains("shell metacharacters and redirection are blocked by Lattice")
        || error.contains("parent-directory path traversal is blocked by Lattice")
        || error.contains("command path escapes the Box through a symlink")
}

fn normalize_tool_call(mut call: ToolCall, mcp_tools: &[McpToolSpec]) -> ToolCall {
    call.name = normalize_tool_name(&call.name, mcp_tools);
    call
}

fn normalize_tool_name(name: &str, mcp_tools: &[McpToolSpec]) -> String {
    let raw = name.trim();
    if raw.is_empty() {
        return name.to_string();
    }
    if is_known_tool_name(raw, mcp_tools) {
        return raw.to_string();
    }
    if let Some(tool_name) = builtin_tool_name(raw) {
        return tool_name.to_string();
    }
    for separator in ['.', '/', ':'] {
        if let Some((_, candidate)) = raw.rsplit_once(separator) {
            let candidate = candidate.trim();
            if is_known_tool_name(candidate, mcp_tools) {
                return candidate.to_string();
            }
            if let Some(tool_name) = builtin_tool_name(candidate) {
                return tool_name.to_string();
            }
        }
    }
    raw.to_string()
}

fn is_known_tool_name(name: &str, mcp_tools: &[McpToolSpec]) -> bool {
    BOX_TOOL_NAMES.iter().any(|tool| *tool == name)
        || mcp_tools.iter().any(|tool| tool.function_name == name)
}

fn builtin_tool_name(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase();
    BOX_TOOL_NAMES
        .iter()
        .copied()
        .find(|tool| *tool == lower.as_str())
}

fn execute_tool_calls(
    paths: &AppPaths,
    config: &AppConfig,
    toolbox: &BoxTools<'_>,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    calls: Vec<ToolCall>,
    control: &AgentControl,
    observer: &mut dyn AgentObserver,
) -> Result<Vec<CompletedToolCall>, String> {
    let calls = calls
        .into_iter()
        .map(|call| normalize_tool_call(call, mcp_tools))
        .collect::<Vec<_>>();
    let requested_tools = calls
        .iter()
        .map(|call| call.name.clone())
        .collect::<Vec<_>>();
    append_agent_checkpoint(
        store,
        project_key,
        session_id,
        "tool_batch_start",
        json!({
            "count": requested_tools.len(),
            "tools": requested_tools,
        }),
    )?;

    if calls.len() > 1 && calls.iter().all(|call| is_parallel_safe_tool(&call.name)) {
        for call in &calls {
            control.check()?;
            let summary = tool_call_summary(call);
            run_lifecycle_hooks(
                paths,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                "pre_tool_use",
                hook_context_for_tool(session_id, call, "pending"),
            );
            observer.on_event(AgentEvent::ToolCallStart {
                call_id: &call.call_id,
                name: &call.name,
                arguments: &call.arguments,
                summary: &summary,
            });
        }

        let parallel_results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for call in calls {
                let call_for_thread = call.clone();
                handles.push(scope.spawn(move || {
                    let result = dispatch_parallel_safe_tool(
                        toolbox,
                        box_id,
                        project_key,
                        mcp_registry,
                        mcp_tools,
                        call_for_thread.clone(),
                    );
                    (call_for_thread, result)
                }));
            }

            let mut results = Vec::new();
            for handle in handles {
                match handle.join() {
                    Ok(result) => results.push(result),
                    Err(_) => return Err("parallel tool worker panicked".to_string()),
                }
            }
            Ok(results)
        })?;

        let mut completed = Vec::new();
        for (call, result) in parallel_results {
            control.check()?;
            let result = result.unwrap_or_else(|err| recoverable_tool_error(&call.name, &err));
            run_lifecycle_hooks(
                paths,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                "post_tool_use",
                hook_context_for_tool(session_id, &call, tool_result_status(&result)),
            );
            observer.on_event(AgentEvent::ToolCallResult {
                call_id: &call.call_id,
                name: &call.name,
                output: &result.ui_output,
            });
            store.append_command(
                project_key,
                session_id,
                &["tool.call".to_string(), call.name.clone()],
                "ok",
                Some(0),
                &result.ui_output,
                "",
            )?;
            let summary = tool_call_summary(&call);
            completed.push(CompletedToolCall {
                call,
                result,
                summary,
            });
        }
        append_completed_tools_checkpoint(store, project_key, session_id, &completed)?;
        return Ok(completed);
    }

    let mut completed = Vec::new();
    for call in calls {
        control.check()?;
        let summary = tool_call_summary(&call);
        run_lifecycle_hooks(
            paths,
            toolbox,
            store,
            project_key,
            session_id,
            box_id,
            "pre_tool_use",
            hook_context_for_tool(session_id, &call, "pending"),
        );
        observer.on_event(AgentEvent::ToolCallStart {
            call_id: &call.call_id,
            name: &call.name,
            arguments: &call.arguments,
            summary: &summary,
        });
        let result = match dispatch_tool(
            paths,
            config,
            toolbox,
            store,
            project_key,
            session_id,
            box_id,
            mcp_registry,
            mcp_tools,
            call.clone(),
        ) {
            Ok(result) => attach_post_edit_verifier(
                paths,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                &call,
                result,
                observer,
            )?,
            Err(err) => recoverable_tool_error(&call.name, &err),
        };
        control.check()?;
        run_lifecycle_hooks(
            paths,
            toolbox,
            store,
            project_key,
            session_id,
            box_id,
            "post_tool_use",
            hook_context_for_tool(session_id, &call, tool_result_status(&result)),
        );
        observer.on_event(AgentEvent::ToolCallResult {
            call_id: &call.call_id,
            name: &call.name,
            output: &result.ui_output,
        });
        store.append_command(
            project_key,
            session_id,
            &["tool.call".to_string(), call.name.clone()],
            "ok",
            Some(0),
            &result.ui_output,
            "",
        )?;
        completed.push(CompletedToolCall {
            call,
            result,
            summary,
        });
    }
    append_completed_tools_checkpoint(store, project_key, session_id, &completed)?;
    Ok(completed)
}

fn attach_post_edit_verifier(
    paths: &AppPaths,
    toolbox: &BoxTools<'_>,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    box_id: &str,
    call: &ToolCall,
    mut result: ToolDispatchResult,
    observer: &mut dyn AgentObserver,
) -> Result<ToolDispatchResult, String> {
    if !is_edit_tool(&call.name) {
        return Ok(result);
    }

    observer.on_event(AgentEvent::Status(
        "running verifier pipeline after file edit",
    ));
    let edited_paths = edited_paths_for_call(call);
    let verifier = VerifierPipeline::new(toolbox.sandbox(), box_id);
    let report = verifier.run_after_edit(&call.name, &edited_paths);
    let report_text = report.to_model_output();
    store.append_command(
        project_key,
        session_id,
        &["verifier.pipeline".to_string(), call.name.clone()],
        report.status.as_str(),
        Some(report.exit_code()),
        &report_text,
        "",
    )?;
    append_agent_checkpoint(
        store,
        project_key,
        session_id,
        "verifier_pipeline",
        verifier_checkpoint_payload(&report),
    )?;
    run_lifecycle_hooks(
        paths,
        toolbox,
        store,
        project_key,
        session_id,
        box_id,
        "file_changed",
        HookContext::default()
            .session(session_id)
            .tool(&call.name)
            .status(report.status.as_str())
            .paths(&edited_paths),
    );

    result.model_output.push_str("\n\n");
    result.model_output.push_str(&report_text);
    result.ui_output.push_str("\n\n");
    result.ui_output.push_str(&report_text);
    Ok(result)
}

fn hook_context_for_tool(session_id: &str, call: &ToolCall, status: &str) -> HookContext {
    HookContext::default()
        .session(session_id)
        .tool(&call.name)
        .status(status)
        .paths(&edited_paths_for_call(call))
        .payload(&call.arguments.to_string())
}

fn tool_result_status(result: &ToolDispatchResult) -> &'static str {
    let lower = result.model_output.to_ascii_lowercase();
    if lower.contains("lattice blocked `")
        || lower.contains("command approval required")
        || lower.contains("approval denied")
        || lower.contains("status: blocked")
    {
        "blocked"
    } else if lower.contains("tool error in `")
        || lower.contains("watchdog stopped `")
        || lower.contains("verifier pipeline\nstatus: failed")
    {
        "failed"
    } else {
        "ok"
    }
}

fn append_completed_tools_checkpoint(
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    completed: &[CompletedToolCall],
) -> Result<(), String> {
    append_agent_checkpoint(
        store,
        project_key,
        session_id,
        "tool_batch_done",
        json!({
            "count": completed.len(),
            "tools": completed
                .iter()
                .map(|completed| json!({
                    "name": completed.call.name.clone(),
                    "call_id": completed.call.call_id.clone(),
                    "summary": completed.summary.clone(),
                    "evidence": tool_checkpoint_excerpt(&completed.result.model_output),
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

fn append_agent_checkpoint(
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    phase: &str,
    payload: Value,
) -> Result<(), String> {
    let content = serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string());
    store.append_checkpoint(project_key, session_id, phase, &content)
}

fn append_agent_checkpoint_best_effort(
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    phase: &str,
    payload: Value,
) {
    let _ = append_agent_checkpoint(store, project_key, session_id, phase, payload);
}

fn run_lifecycle_hooks(
    paths: &AppPaths,
    toolbox: &BoxTools<'_>,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    box_id: &str,
    event: &str,
    context: HookContext,
) -> Vec<HookExecution> {
    let hooks = match HookStore::new(paths) {
        Ok(hooks) => hooks,
        Err(err) => {
            append_hook_runtime_error(store, project_key, session_id, event, &err);
            return Vec::new();
        }
    };
    let executions = match hooks.run_event_with_context(toolbox.sandbox(), box_id, event, &context)
    {
        Ok(executions) => executions,
        Err(err) => {
            append_hook_runtime_error(store, project_key, session_id, event, &err);
            return Vec::new();
        }
    };
    for execution in &executions {
        let mut output = String::new();
        output.push_str("Hook ");
        output.push_str(&execution.id);
        output.push_str(" :: ");
        output.push_str(&execution.event);
        output.push_str("\ncommand: ");
        output.push_str(&execution.command.join(" "));
        if !execution.output.trim().is_empty() {
            output.push_str("\n\n");
            output.push_str(&execution.output);
        }
        let _ = store.append_command(
            project_key,
            session_id,
            &[
                "hook.event".to_string(),
                execution.event.clone(),
                execution.id.clone(),
            ],
            &execution.status,
            execution.exit_code.map(i64::from),
            &output,
            "",
        );
    }
    executions
}

fn append_hook_runtime_error(
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    event: &str,
    error: &str,
) {
    let _ = store.append_command(
        project_key,
        session_id,
        &["hook.event".to_string(), event.to_string()],
        "blocked",
        Some(1),
        "",
        &format!("hook runtime error did not stop the agent: {error}"),
    );
}

fn verifier_checkpoint_payload(report: &VerifierReport) -> Value {
    json!({
        "trigger": report.trigger.clone(),
        "status": report.status.as_str(),
        "edited_paths": report.edited_paths.clone(),
        "undo_hint": report.undo_hint.clone(),
        "skipped": report.skipped.clone(),
        "commands": report.commands
            .iter()
            .map(|command| json!({
                "label": command.label.clone(),
                "command": command.command.clone(),
                "status": command.status.as_str(),
                "exit_code": command.exit_code,
            }))
            .collect::<Vec<_>>(),
    })
}

fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "apply_patch"
            | "write_file"
            | "replace_in_file"
            | "delete_file"
            | "copy_file"
            | "move_file"
    )
}

fn edited_paths_for_call(call: &ToolCall) -> Vec<String> {
    let mut paths = Vec::new();
    match call.name.as_str() {
        "apply_patch" => {
            if let Some(patch) = call
                .arguments
                .get("patch_text")
                .and_then(|value| value.as_str())
            {
                paths.extend(patch_edited_paths(patch));
            }
        }
        "write_file" | "replace_in_file" | "delete_file" => {
            push_argument_path(&mut paths, &call.arguments, "path");
        }
        "copy_file" | "move_file" => {
            push_argument_path(&mut paths, &call.arguments, "source_path");
            push_argument_path(&mut paths, &call.arguments, "destination_path");
        }
        _ => {}
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_argument_path(paths: &mut Vec<String>, arguments: &Value, key: &str) {
    if let Some(path) = arguments
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        paths.push(path.to_string());
    }
}

fn patch_edited_paths(patch_text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch_text.lines() {
        for prefix in [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = line.strip_prefix(prefix) {
                let path = path.trim();
                if !path.is_empty() {
                    paths.push(path.to_string());
                }
            }
        }
    }
    paths
}

fn tool_call_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" => command_summary(&call.arguments, "command", ""),
        "git" => command_summary(&call.arguments, "command", "git"),
        "gh" => command_summary(&call.arguments, "command", "gh"),
        "apply_patch" => call
            .arguments
            .get("patch_text")
            .and_then(|value| value.as_str())
            .map(patch_target_summary)
            .unwrap_or_else(|| "patch".to_string()),
        "navigate" | "list_dir" | "read_file" | "write_file" | "read_lines" | "grep_lines"
        | "head_lines" | "tail_lines" | "delete_file" => call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or(".")
            .to_string(),
        "glob_files" => call
            .arguments
            .get("pattern")
            .and_then(|value| value.as_str())
            .unwrap_or("*")
            .to_string(),
        "search" => call
            .arguments
            .get("pattern")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        "lab_learn" => call
            .arguments
            .get("content")
            .and_then(|value| value.as_str())
            .map(|value| truncate_inline(value, 80))
            .unwrap_or_else(|| "preference".to_string()),
        "lab_recall" => call
            .arguments
            .get("query")
            .and_then(|value| value.as_str())
            .map(|value| truncate_inline(value, 80))
            .unwrap_or_default(),
        "copy_file" | "move_file" => {
            let source = call
                .arguments
                .get("source_path")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let destination = call
                .arguments
                .get("destination_path")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            format!("{source} -> {destination}")
        }
        "replace_in_file" => {
            let path = call
                .arguments
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let find = call
                .arguments
                .get("find")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if find.is_empty() {
                path.to_string()
            } else {
                format!("{path} · replace `{}`", truncate_inline(find, 32))
            }
        }
        name if name.starts_with("mcp__") => mcp_call_summary(name),
        _ => String::new(),
    }
}

fn command_summary(arguments: &Value, key: &str, prefix: &str) -> String {
    let mut command = arguments
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .or_else(|| {
            arguments
                .get(key)
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .unwrap_or_default();
    if !prefix.is_empty() && !command.trim_start().starts_with(prefix) {
        command = format!("{prefix} {command}");
    }
    truncate_inline(command.trim(), 96)
}

fn patch_target_summary(patch_text: &str) -> String {
    let mut files = Vec::new();
    for line in patch_text.lines() {
        let path = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "));
        if let Some(path) = path {
            let path = path.trim();
            if !path.is_empty() && !files.iter().any(|item: &String| item == path) {
                files.push(path.to_string());
            }
        }
    }
    match files.len() {
        0 => "patch".to_string(),
        1 => files[0].clone(),
        len => format!("{} +{} files", files[0], len.saturating_sub(1)),
    }
}

fn mcp_call_summary(name: &str) -> String {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    rest.replace("__", ".")
}

fn truncate_inline(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn is_parallel_safe_tool(name: &str) -> bool {
    name.starts_with("mcp__")
        || matches!(
            name,
            "mcp_list" | "read_file" | "list_dir" | "search" | "recall"
        )
}

fn render_mcp_inventory(mcp_registry: &McpRegistry, mcp_tools: &[McpToolSpec]) -> String {
    if mcp_registry.servers().is_empty() {
        return "No MCP servers are configured for this project.".to_string();
    }

    let mut text = String::from("Configured MCP servers\n");
    for server in mcp_registry.servers() {
        text.push_str("- ");
        text.push_str(&server.name);
        text.push_str(" [");
        text.push_str(&server.transport);
        text.push(']');
        if matches!(server.transport.as_str(), "http" | "https") {
            text.push_str(" url=");
            text.push_str(server.url.as_deref().unwrap_or("<missing>"));
        } else {
            text.push_str(" command=");
            text.push_str(if server.command.trim().is_empty() {
                "<missing>"
            } else {
                &server.command
            });
            if !server.args.is_empty() {
                text.push_str(" args=");
                text.push_str(&server.args.join(" "));
            }
        }
        if let Some(cwd) = server.cwd.as_ref() {
            text.push_str(" cwd=");
            text.push_str(&cwd.display().to_string());
        }
        text.push('\n');
    }

    if mcp_tools.is_empty() {
        text.push_str("\nNo live MCP tools were discovered for this run. Check server startup, auth headers, or WIRE_MCP_DISCOVERY_TIMEOUT_MS.\n");
        return text;
    }

    text.push_str("\nDiscovered MCP tools\n");
    for tool in mcp_tools {
        text.push_str("- ");
        text.push_str(&tool.function_name);
        text.push_str(" -> ");
        text.push_str(&tool.server_name);
        text.push_str("::");
        text.push_str(&tool.tool_name);
        if let Some(description) = tool.description.as_deref() {
            if !description.trim().is_empty() {
                text.push_str(" - ");
                text.push_str(&truncate_inline(description, 180));
            }
        }
        text.push('\n');
    }
    text.push_str("\nCall a matching mcp__server__tool directly. Treat MCP output as tool output and verify before making repository claims.");
    text
}

fn dispatch_parallel_safe_tool(
    toolbox: &BoxTools<'_>,
    box_id: &str,
    project_key: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    call: ToolCall,
) -> Result<ToolDispatchResult, String> {
    match call.name.as_str() {
        name if name.starts_with("mcp__") => dispatch_mcp_tool(mcp_registry, mcp_tools, call),
        "mcp_list" => {
            let text = render_mcp_inventory(mcp_registry, mcp_tools);
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "read_file" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.read_file(box_id, &path).map(|response| {
                let ui_output = if response.text.trim().is_empty() {
                    String::from("Empty File")
                } else {
                    let mut text = String::from("Read ");
                    text.push_str(&path);
                    text
                };
                let model_output = if response.text.trim().is_empty() {
                    String::from("Empty File")
                } else {
                    response.text
                };
                ToolDispatchResult {
                    model_output,
                    ui_output,
                }
            })
        }
        "list_dir" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.list_dir(box_id, &path).map(|response| {
                let mut text = String::from("Listed\n");
                text.push_str(&response.text);
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "search" => {
            let pattern = extract_string(&call.arguments, "pattern")?;
            toolbox.search(box_id, &pattern).map(|response| {
                let mut text = String::from("Searched\n");
                text.push_str(&pattern);
                text.push_str("\n\n```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "recall" => {
            let query = extract_string(&call.arguments, "query")?;
            toolbox.recall(project_key, &query).map(|response| {
                let mut text = String::from("Context Recovered From Other Session\n");
                text.push_str("query: ");
                text.push_str(&redact_secrets(&query));
                text.push_str("\n\n```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        other => Err(format!("tool is not parallel safe: {other}")),
    }
}

fn permission_mode_instructions(permission_mode: PermissionMode) -> String {
    match permission_mode {
        PermissionMode::Normal => {
            "Active permission mode: Normal. The Box is the project root plus every subdirectory under it. File edits and allowed development commands are allowed only inside that tree. Lattice blocks path escape, privilege escalation, and host service changes. Direct network tools, direct shells, inline interpreter execution, permission changes, and long-running listeners require an approval request before execution. Regex metacharacters inside argv arguments are allowed. Prefer `search`, `grep_lines`, `read_file`, `read_lines`, and `glob_files` for inspection. Project-relative paths and `/workspace/...` paths are valid when they stay inside the Box.".to_string()
        }
        PermissionMode::Guardian => {
            "Active permission mode: Guardian. File tools and edits stay inside the Box. Commands are first checked by Lattice and local approval state, then reviewed through the configured provider using the command, reason, and context. Include a concise `reason` when calling shell, git, or gh. Prefer native inspection tools over shell wrappers. If approval or Guardian denies a command, choose a narrower safer command instead of retrying the same one.".to_string()
        }
        PermissionMode::FullAccess => {
            "Active permission mode: Full Access. Not recommended. File access, command execution, and network activity are unrestricted on the host and are not reviewed by Guardian. Keep the requested task scope and avoid destructive host operations unless explicitly required.".to_string()
        }
    }
}

fn dynamic_tooling_instructions(paths: &AppPaths, mcp_tools: &[McpToolSpec]) -> String {
    let mut out = String::from("Dynamic tooling context:\n");
    match SkillStore::new(paths).and_then(|store| store.list()) {
        Ok(skills) if !skills.is_empty() => {
            out.push_str("- Local skills are installed. If the current task matches a skill description, call `skill_read` before following that workflow.\n");
            for skill in skills.iter().take(16) {
                out.push_str("  - ");
                out.push_str(&skill.name);
                if !skill.description.trim().is_empty() {
                    out.push_str(": ");
                    out.push_str(&truncate_inline(&skill.description, 160));
                }
                out.push('\n');
            }
        }
        Ok(_) => {
            out.push_str("- No local skills are installed yet. If you discover a stable repeatable workflow, use `skill_create` after evidence exists.\n");
        }
        Err(err) => {
            out.push_str("- Skill discovery failed: ");
            out.push_str(&truncate_inline(&err, 160));
            out.push('\n');
        }
    }

    if mcp_tools.is_empty() {
        out.push_str("- No live MCP tools were discovered for this run. `mcp_list` can still show configured servers and discovery problems.\n");
    } else {
        out.push_str("- Live MCP tools are available. Prefer the exact namespaced MCP tool over shell when it directly fits the task.\n");
        for tool in mcp_tools.iter().take(24) {
            out.push_str("  - ");
            out.push_str(&tool.function_name);
            if let Some(description) = tool
                .description
                .as_deref()
                .filter(|description| !description.trim().is_empty())
            {
                out.push_str(": ");
                out.push_str(&truncate_inline(description, 160));
            }
            out.push('\n');
        }
    }
    out.push_str("- AFUP is active through `lab_learn` and `lab_recall`. Learn only durable user patterns or repeated workflow preferences, and use recalled adaptation guidance only when it is relevant to the current task.\n");
    out
}

async fn discover_mcp_tools_blocking(registry: McpRegistry) -> crate::mcp::McpDiscoveryReport {
    tokio::task::spawn_blocking(move || registry.discover_tools_report())
        .await
        .ok()
        .map(|report| report)
        .unwrap_or_else(|| crate::mcp::McpDiscoveryReport {
            tools: Vec::new(),
            errors: vec!["MCP discovery worker panicked".to_string()],
        })
}

fn responses_input_value(text: &str, images: &[PromptImage]) -> Value {
    let text = redact_secrets(text);
    if images.is_empty() {
        return json!(text);
    }

    let mut content = vec![json!({
        "type": "input_text",
        "text": text
    })];
    for image in images {
        content.push(json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data_base64),
            "detail": "auto"
        }));
    }

    json!([
        {
            "role": "user",
            "content": content
        }
    ])
}

#[derive(Debug, Default)]
struct ChatStreamTurn {
    text: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<TokenUsage>,
    finish_reason: Option<String>,
    native_finish_reason: Option<String>,
}

fn chat_user_message(input: Value) -> Value {
    match input {
        Value::String(text) => json!({ "role": "user", "content": text }),
        Value::Array(items) => {
            if let Some(first) = items.first() {
                if first.get("role").and_then(|value| value.as_str()) == Some("user") {
                    let content = first
                        .get("content")
                        .and_then(|value| value.as_array())
                        .map(|parts| {
                            Value::Array(
                                parts
                                    .iter()
                                    .filter_map(chat_content_part_from_responses_part)
                                    .collect(),
                            )
                        })
                        .unwrap_or_else(|| {
                            json!(first
                                .get("content")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default())
                        });
                    return json!({ "role": "user", "content": content });
                }
            }
            json!({ "role": "user", "content": Value::Array(items).to_string() })
        }
        other => json!({ "role": "user", "content": other.to_string() }),
    }
}

fn chat_content_part_from_responses_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(|value| value.as_str()) {
        Some("input_text") => Some(json!({
            "type": "text",
            "text": part.get("text").and_then(|value| value.as_str()).unwrap_or_default()
        })),
        Some("input_image") => Some(json!({
            "type": "image_url",
            "image_url": {
                "url": part.get("image_url").and_then(|value| value.as_str()).unwrap_or_default()
            }
        })),
        _ => None,
    }
}

fn chat_tool_definitions(paths: &AppPaths, mcp_tools: &[McpToolSpec]) -> Value {
    let Value::Array(tools) = tool_definitions(paths, mcp_tools) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        tools
            .into_iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.clone();
                let description = tool.get("description").cloned().unwrap_or(Value::Null);
                let parameters = tool.get("parameters").cloned().unwrap_or_else(|| {
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })
                });
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters
                    }
                }))
            })
            .collect(),
    )
}

fn chat_assistant_tool_calls(calls: &[ToolCall]) -> Value {
    Value::Array(
        calls
            .iter()
            .map(|call| {
                let arguments = match &call.arguments {
                    Value::String(raw) => raw.clone(),
                    value => value.to_string(),
                };
                json!({
                    "id": call.call_id.clone(),
                    "type": "function",
                    "function": {
                        "name": call.name.clone(),
                        "arguments": arguments
                    }
                })
            })
            .collect(),
    )
}

fn apply_reasoning_effort(body: &mut Value, config: &AppConfig) {
    let Some(effort) = config
        .model_reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    body["reasoning_effort"] = json!(effort);
}

async fn run_anthropic_agent_loop(
    paths: &AppPaths,
    config: &AppConfig,
    first_input: Value,
    toolbox: &BoxTools<'_>,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    control: &AgentControl,
    observer: &mut dyn AgentObserver,
    instructions: String,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let tools = anthropic_tool_definitions(paths, mcp_tools);
    let mut messages = vec![anthropic_user_message(first_input)];
    let mut last_text = String::new();
    let mut last_tool_summary: Option<String> = None;
    let mut last_tool_fingerprint: Option<String> = None;
    let mut repeated_tool_turns = 0usize;
    let mut awaiting_final_answer_after_tools = false;
    let mut grounding_repair_sent = false;
    let mut empty_tool_continuations = 0usize;
    let mut parser_recoveries = 0usize;
    let mut provider_request_recoveries = 0usize;

    for _ in 0..MAX_AGENT_BACKEND_TURNS {
        control.check()?;
        store.append_command(
            project_key,
            session_id,
            &[
                "anthropic.messages.create".to_string(),
                config.model.clone(),
            ],
            "ok",
            Some(0),
            &format!(
                "messages={} tools={}",
                messages.len(),
                BOX_TOOL_NAMES.join(",")
            ),
            "",
        )?;
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_request",
            json!({
                "backend": "anthropic_messages",
                "model": config.model.clone(),
                "messages": messages.len(),
                "tools": BOX_TOOL_NAMES,
            }),
        )?;
        observer.on_event(AgentEvent::Status(
            "sending request to anthropic messages backend",
        ));
        let mut body = json!({
            "model": config.model,
            "system": instructions,
            "messages": messages,
            "max_tokens": 4096,
            "stream": true,
        });
        body["tools"] = tools.clone();

        let response = match client
            .post(format!(
                "{}/messages",
                config.base_url.trim_end_matches('/')
            ))
            .header(ACCEPT, "text/event-stream")
            .header(ACCEPT_ENCODING, "identity")
            .headers(provider_headers(config)?)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let message = format!("anthropic messages request failed before stream: {err}");
                match provider_request_error_after_tools_action(
                    "anthropic_messages",
                    "anthropic messages endpoint",
                    &message,
                    awaiting_final_answer_after_tools,
                    last_tool_summary.as_deref(),
                    &mut provider_request_recoveries,
                    store,
                    project_key,
                    session_id,
                    observer,
                ) {
                    ProviderRequestErrorAction::Retry => {
                        tokio::time::sleep(Duration::from_millis(PROVIDER_REQUEST_RETRY_DELAY_MS))
                            .await;
                        continue;
                    }
                    ProviderRequestErrorAction::Finish(text) => return Ok(text),
                    ProviderRequestErrorAction::None => {}
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "anthropic_messages",
                        "stage": "request",
                        "error": message.clone(),
                    }),
                );
                return Err(message);
            }
        };
        control.check()?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = upstream_status_error("anthropic messages endpoint", status, &text);
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_error",
                json!({
                    "backend": "anthropic_messages",
                    "stage": "status",
                    "status": status.as_u16(),
                    "error": message.clone(),
                }),
            );
            return Err(message);
        }

        let turn_result = {
            let mut stream_recorder =
                StreamCheckpointRecorder::new(store, project_key, session_id, "anthropic_messages");
            let result =
                consume_anthropic_stream(response, observer, control, &mut stream_recorder).await;
            if result.is_ok() {
                stream_recorder.flush("stream_completed");
            } else {
                stream_recorder.flush("stream_error_checkpoint");
            }
            result
        };
        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                if parser_recoveries < MAX_WDF_PARSER_RECOVERIES && recoverable_parser_error(&err) {
                    parser_recoveries += 1;
                    observer.on_event(AgentEvent::Status(
                        "ACP/WDF stopped malformed anthropic stream and requested a clean continuation",
                    ));
                    let repair_prompt = acp::wdf_parser_error_prompt("anthropic_messages", &err);
                    let assessment = acp::AcpAssessment::wdf(
                        "anthropic_messages",
                        if awaiting_final_answer_after_tools {
                            AcpPhase::AwaitingAssistantAfterTools
                        } else {
                            AcpPhase::AwaitingAssistant
                        },
                        acp::AcpViolation::ParserStreamError,
                        "parser stopped malformed stream and dropped the request",
                        repair_prompt.clone(),
                    );
                    append_acp_wdf_checkpoint(
                        store,
                        project_key,
                        session_id,
                        &assessment,
                        parser_recoveries,
                    );
                    messages.push(json!({
                        "role": "user",
                        "content": repair_prompt
                    }));
                    continue;
                }
                append_agent_checkpoint_best_effort(
                    store,
                    project_key,
                    session_id,
                    "provider_error",
                    json!({
                        "backend": "anthropic_messages",
                        "stage": "stream",
                        "error": err.clone(),
                    }),
                );
                return Err(err);
            }
        };
        provider_request_recoveries = 0;
        append_agent_checkpoint(
            store,
            project_key,
            session_id,
            "model_turn",
            json!({
                "backend": "anthropic_messages",
                "text_chars": turn.text.chars().count(),
                "tool_calls": turn.tool_calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
                "usage": turn.usage.clone(),
            }),
        )?;
        if let Some(usage) = turn.usage.clone() {
            observer.on_event(AgentEvent::Usage(usage));
        }

        if !turn.tool_calls.is_empty() {
            messages.push(json!({
                "role": "assistant",
                "content": turn.assistant_blocks,
            }));
            let completed_calls = execute_tool_calls(
                paths,
                config,
                toolbox,
                store,
                project_key,
                session_id,
                box_id,
                mcp_registry,
                mcp_tools,
                turn.tool_calls,
                control,
                observer,
            )?;
            let mut turn_tool_names = Vec::new();
            let mut turn_tool_outputs = Vec::new();
            let mut tool_results = Vec::new();
            for completed in &completed_calls {
                turn_tool_names.push(completed.call.name.clone());
                turn_tool_outputs.push(completed.result.model_output.clone());
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": completed.call.call_id.clone(),
                    "content": completed.result.model_output.clone(),
                }));
            }
            messages.push(json!({
                "role": "user",
                "content": tool_results,
            }));

            last_tool_summary = Some(summarize_completed_tool_turn(&completed_calls));
            let fingerprint = tool_turn_fingerprint(&turn_tool_names, &turn_tool_outputs);
            if last_tool_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                repeated_tool_turns = repeated_tool_turns.saturating_add(1);
            } else {
                repeated_tool_turns = 1;
                last_tool_fingerprint = Some(fingerprint);
            }
            if repeated_tool_turns >= MAX_REPEATED_IDENTICAL_TOOL_TURNS {
                let summary = last_tool_summary.clone().unwrap_or_default();
                if tool_outputs_need_repair(&turn_tool_outputs) {
                    observer.on_event(AgentEvent::Status(
                        "repeated recoverable tool error; asking model to choose another route",
                    ));
                    append_agent_checkpoint_best_effort(
                        store,
                        project_key,
                        session_id,
                        "tool_repair_prompt",
                        json!({
                            "backend": "anthropic_messages",
                            "reason": "repeated_recoverable_tool_error",
                            "summary": summary.clone(),
                        }),
                    );
                    messages.push(json!({
                        "role": "user",
                        "content": repeated_tool_repair_prompt(&summary)
                    }));
                    repeated_tool_turns = 0;
                    last_tool_fingerprint = None;
                    empty_tool_continuations = 0;
                    awaiting_final_answer_after_tools = true;
                    continue;
                }
                last_text = format!(
                    "Loop guard stopped repeated identical tool execution.\n\n{}",
                    summary
                );
                break;
            }
            awaiting_final_answer_after_tools = true;
            observer.on_event(AgentEvent::Status(
                "sending tool results to anthropic messages backend",
            ));
            empty_tool_continuations = 0;
            continue;
        }

        if !turn.text.trim().is_empty() {
            if let Some(repair_prompt) =
                grounding_repair_prompt(paths, &turn.text, last_tool_summary.as_deref())
            {
                if grounding_repair_sent {
                    last_text = grounding_blocked_text(&grounding_violations(paths, &turn.text));
                    break;
                }
                messages.push(json!({
                    "role": "user",
                    "content": repair_prompt
                }));
                grounding_repair_sent = true;
                continue;
            }
            last_text = turn.text;
            break;
        }

        if awaiting_final_answer_after_tools {
            let summary = last_tool_summary
                .clone()
                .unwrap_or_else(|| "No tool summary available.".to_string());
            if empty_tool_continuations < MAX_EMPTY_TOOL_CONTINUATIONS {
                empty_tool_continuations += 1;
                observer.on_event(AgentEvent::Status(
                    "ACP/WDF recovered empty assistant turn after tool results",
                ));
                let assessment = acp::assess_assistant_turn(
                    "anthropic_messages",
                    AcpPhase::AwaitingAssistantAfterTools,
                    "",
                    0,
                    Some(&summary),
                );
                append_acp_wdf_checkpoint(
                    store,
                    project_key,
                    session_id,
                    &assessment,
                    empty_tool_continuations,
                );
                let continuation = assessment
                    .repair_prompt
                    .clone()
                    .unwrap_or_else(|| tool_continuation_prompt(&summary));
                messages.push(json!({
                    "role": "user",
                    "content": continuation
                }));
                continue;
            }
            observer.on_event(AgentEvent::Status(
                "stopped after empty provider turn without final text",
            ));
            append_agent_checkpoint_best_effort(
                store,
                project_key,
                session_id,
                "provider_empty_stop",
                json!({
                    "backend": "anthropic_messages",
                    "summary": summary.clone(),
                }),
            );
            last_text = tool_checkpoint_after_empty_provider_text(&summary);
            break;
        }

        if let Some(summary) = last_tool_summary {
            return Err(empty_stream_after_tools_error(
                "anthropic messages endpoint",
                &summary,
            ));
        }
        return Err(empty_stream_error("anthropic messages endpoint"));
    }

    if last_text.is_empty() {
        if let Some(summary) = last_tool_summary {
            return Ok(tool_checkpoint_after_empty_provider_text(&summary));
        }
        return Err("agent completed without a final text response".to_string());
    }

    Ok(last_text)
}

fn anthropic_tool_definitions(paths: &AppPaths, mcp_tools: &[McpToolSpec]) -> Value {
    let Value::Array(tools) = tool_definitions(paths, mcp_tools) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        tools
            .into_iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.clone();
                let description = tool.get("description").cloned().unwrap_or(Value::Null);
                let input_schema = tool.get("parameters").cloned().unwrap_or_else(|| {
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })
                });
                Some(json!({
                    "name": name,
                    "description": description,
                    "input_schema": input_schema
                }))
            })
            .collect(),
    )
}

fn anthropic_user_message(input: Value) -> Value {
    match input {
        Value::String(text) => json!({ "role": "user", "content": text }),
        Value::Array(items) => {
            if let Some(first) = items.first() {
                if first.get("role").and_then(|value| value.as_str()) == Some("user") {
                    let content = first
                        .get("content")
                        .and_then(|value| value.as_array())
                        .map(|parts| {
                            Value::Array(
                                parts
                                    .iter()
                                    .filter_map(anthropic_content_part_from_responses_part)
                                    .collect(),
                            )
                        })
                        .unwrap_or_else(|| {
                            json!(first
                                .get("content")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default())
                        });
                    return json!({ "role": "user", "content": content });
                }
            }
            json!({ "role": "user", "content": Value::Array(items).to_string() })
        }
        other => json!({ "role": "user", "content": other.to_string() }),
    }
}

fn anthropic_content_part_from_responses_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(|value| value.as_str()) {
        Some("input_text") => Some(json!({
            "type": "text",
            "text": part.get("text").and_then(|value| value.as_str()).unwrap_or_default()
        })),
        Some("input_image") => data_url_to_anthropic_image(
            part.get("image_url")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

fn data_url_to_anthropic_image(data_url: &str) -> Option<Value> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": mime_type,
            "data": data
        }
    }))
}

#[derive(Debug, Default)]
struct AnthropicStreamTurn {
    text: String,
    tool_calls: Vec<ToolCall>,
    assistant_blocks: Vec<Value>,
    usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Default)]
struct AnthropicPendingBlock {
    kind: String,
    id: String,
    name: String,
    text: String,
    input_json: String,
    input_seed: Option<Value>,
}

async fn consume_anthropic_stream(
    response: reqwest::Response,
    observer: &mut dyn AgentObserver,
    control: &AgentControl,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<AnthropicStreamTurn, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = AnthropicStreamTurn::default();
    let mut pending_blocks: HashMap<usize, AnthropicPendingBlock> = HashMap::new();

    loop {
        control.check()?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|e| {
            format!(
                "anthropic messages stream failed while reading SSE body: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        let chunk = str::from_utf8(&chunk).map_err(|e| {
            format!(
                "anthropic messages stream returned non-UTF8 SSE bytes: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        buffer.push_str(chunk);

        while let Some(pos) = buffer.find("\n\n") {
            let event_block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_block.trim().is_empty() {
                continue;
            }
            handle_anthropic_sse_block(
                &event_block,
                &mut state,
                &mut pending_blocks,
                observer,
                stream_recorder,
            )?;
        }
    }

    if !buffer.trim().is_empty() {
        handle_anthropic_sse_block(
            &buffer,
            &mut state,
            &mut pending_blocks,
            observer,
            stream_recorder,
        )?;
    }
    finalize_anthropic_pending_blocks(&mut state, pending_blocks);
    Ok(state)
}

fn handle_anthropic_sse_block(
    event_block: &str,
    state: &mut AnthropicStreamTurn,
    pending_blocks: &mut HashMap<usize, AnthropicPendingBlock>,
    observer: &mut dyn AgentObserver,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<(), String> {
    let Some((_event_name, payload)) = extract_sse_event(event_block) else {
        return Ok(());
    };
    if payload == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(&payload).map_err(|e| {
        format!(
            "anthropic messages stream returned invalid JSON event: {e}; payload={}",
            preview_text(&payload, 500)
        )
    })?;
    if let Some(error) = value.get("error") {
        return Err(format!("anthropic messages stream reported error: {error}"));
    }
    if let Some(usage) = parse_anthropic_usage(&value) {
        state.usage = Some(usage);
    }

    match value.get("type").and_then(|value| value.as_str()) {
        Some("content_block_start") => {
            let index = value
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let block = value
                .get("content_block")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let kind = block
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let mut pending = AnthropicPendingBlock {
                kind,
                ..AnthropicPendingBlock::default()
            };
            if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                pending.text.push_str(text);
            }
            if let Some(id) = block.get("id").and_then(|value| value.as_str()) {
                pending.id = id.to_string();
            }
            if let Some(name) = block.get("name").and_then(|value| value.as_str()) {
                pending.name = name.to_string();
            }
            if let Some(input) = block.get("input") {
                pending.input_seed = Some(input.clone());
            }
            if pending.kind == "tool_use"
                && (!pending.name.is_empty() || pending.input_seed.is_some())
            {
                let input_seed = pending
                    .input_seed
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default();
                observer.on_event(AgentEvent::ToolCallDelta {
                    call_id: (!pending.id.is_empty()).then_some(pending.id.as_str()),
                    name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    arguments_delta: &input_seed,
                });
                stream_recorder.record_tool_delta(
                    (!pending.id.is_empty()).then_some(pending.id.as_str()),
                    (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    &input_seed,
                );
            }
            pending_blocks.insert(index, pending);
        }
        Some("content_block_delta") => {
            let index = value
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let delta = value.get("delta").cloned().unwrap_or_else(|| json!({}));
            let pending = pending_blocks.entry(index).or_default();
            match delta.get("type").and_then(|value| value.as_str()) {
                Some("text_delta") => {
                    let text = delta
                        .get("text")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        pending.text.push_str(text);
                        state.text.push_str(text);
                        stream_recorder.record_text_delta(text);
                        observer.on_event(AgentEvent::TextDelta(text));
                    }
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    pending.input_json.push_str(partial);
                    if !partial.is_empty() {
                        observer.on_event(AgentEvent::ToolCallDelta {
                            call_id: (!pending.id.is_empty()).then_some(pending.id.as_str()),
                            name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                            arguments_delta: partial,
                        });
                        stream_recorder.record_tool_delta(
                            (!pending.id.is_empty()).then_some(pending.id.as_str()),
                            (!pending.name.is_empty()).then_some(pending.name.as_str()),
                            partial,
                        );
                    }
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            let index = value
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            if let Some(pending) = pending_blocks.remove(&index) {
                finalize_anthropic_block(state, pending);
            }
        }
        _ => {}
    }
    Ok(())
}

fn finalize_anthropic_pending_blocks(
    state: &mut AnthropicStreamTurn,
    pending_blocks: HashMap<usize, AnthropicPendingBlock>,
) {
    let mut blocks = pending_blocks.into_iter().collect::<Vec<_>>();
    blocks.sort_by_key(|(index, _)| *index);
    for (_index, pending) in blocks {
        finalize_anthropic_block(state, pending);
    }
}

fn finalize_anthropic_block(state: &mut AnthropicStreamTurn, pending: AnthropicPendingBlock) {
    match pending.kind.as_str() {
        "text" => {
            if !pending.text.trim().is_empty() {
                state.assistant_blocks.push(json!({
                    "type": "text",
                    "text": pending.text
                }));
            }
        }
        "tool_use" => {
            if pending.id.is_empty() || pending.name.is_empty() {
                return;
            }
            let input = if pending.input_json.trim().is_empty() {
                pending.input_seed.unwrap_or_else(|| json!({}))
            } else {
                parse_tool_arguments(&pending.input_json)
            };
            state.assistant_blocks.push(json!({
                "type": "tool_use",
                "id": pending.id,
                "name": pending.name,
                "input": input,
            }));
            state.tool_calls.push(ToolCall {
                call_id: pending.id,
                name: pending.name,
                arguments: input,
            });
        }
        _ => {}
    }
}

fn parse_anthropic_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value.get("usage").or_else(|| {
        value
            .get("message")
            .and_then(|message| message.get("usage"))
    })?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
        .saturating_add(
            usage
                .get("cache_creation_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
        )
        .saturating_add(
            usage
                .get("cache_read_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
        );
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens.saturating_add(output_tokens),
    })
}

async fn consume_stream(
    response: reqwest::Response,
    observer: &mut dyn AgentObserver,
    control: &AgentControl,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<StreamTurn, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = StreamTurn::default();
    let mut pending_calls: HashMap<String, PendingCall> = HashMap::new();

    loop {
        control.check()?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|e| {
            format!(
                "responses stream failed while reading SSE body: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        let chunk = str::from_utf8(&chunk).map_err(|e| {
            format!(
                "responses stream returned non-UTF8 SSE bytes: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        buffer.push_str(chunk);

        while let Some(pos) = buffer.find("\n\n") {
            let event_block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_block.trim().is_empty() {
                continue;
            }

            let event_data = extract_sse_event(&event_block);
            let Some((event_name, payload)) = event_data else {
                continue;
            };
            if payload == "[DONE]" {
                continue;
            }

            let mut event: Value = serde_json::from_str(&payload).map_err(|e| {
                format!(
                    "responses stream returned invalid JSON event: {e}; payload={}",
                    preview_text(&payload, 500)
                )
            })?;
            attach_sse_event_type(&mut event, event_name.as_deref());
            handle_stream_event(
                &event,
                &mut state,
                &mut pending_calls,
                observer,
                stream_recorder,
            )?;
        }
    }

    if !buffer.trim().is_empty() {
        if let Some((event_name, payload)) = extract_sse_event(&buffer) {
            if payload != "[DONE]" {
                let mut event: Value = serde_json::from_str(&payload).map_err(|e| {
                    format!(
                        "responses stream returned invalid trailing JSON event: {e}; payload={}",
                        preview_text(&payload, 500)
                    )
                })?;
                attach_sse_event_type(&mut event, event_name.as_deref());
                handle_stream_event(
                    &event,
                    &mut state,
                    &mut pending_calls,
                    observer,
                    stream_recorder,
                )?;
            }
        }
    }

    Ok(state)
}

async fn consume_chat_stream(
    response: reqwest::Response,
    observer: &mut dyn AgentObserver,
    control: &AgentControl,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<ChatStreamTurn, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = ChatStreamTurn::default();
    let mut pending_calls: HashMap<usize, PendingCall> = HashMap::new();

    loop {
        control.check()?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|e| {
            format!(
                "chat completions stream failed while reading SSE body: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        let chunk = str::from_utf8(&chunk).map_err(|e| {
            format!(
                "chat completions stream returned non-UTF8 SSE bytes: {e}{}",
                stream_buffer_preview(&buffer)
            )
        })?;
        buffer.push_str(chunk);

        while let Some(pos) = buffer.find("\n\n") {
            let event_block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_block.trim().is_empty() {
                continue;
            }
            handle_chat_sse_block(
                &event_block,
                &mut state,
                &mut pending_calls,
                observer,
                stream_recorder,
            )?;
        }
    }

    if !buffer.trim().is_empty() {
        handle_chat_sse_block(
            &buffer,
            &mut state,
            &mut pending_calls,
            observer,
            stream_recorder,
        )?;
    }

    finalize_chat_pending_calls(&mut state, pending_calls);
    Ok(state)
}

fn handle_chat_sse_block(
    event_block: &str,
    state: &mut ChatStreamTurn,
    pending_calls: &mut HashMap<usize, PendingCall>,
    observer: &mut dyn AgentObserver,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<(), String> {
    let Some((_event_name, payload)) = extract_sse_event(event_block) else {
        return Ok(());
    };
    if payload == "[DONE]" {
        return Ok(());
    }

    let value: Value = serde_json::from_str(&payload).map_err(|e| {
        format!(
            "chat completions stream returned invalid JSON event: {e}; payload={}",
            preview_text(&payload, 500)
        )
    })?;
    if let Some(error) = value.get("error") {
        return Err(format!("chat completions stream reported error: {error}"));
    }
    if let Some(usage) = parse_usage(&value) {
        state.usage = Some(usage);
    }

    let Some(choices) = value.get("choices").and_then(|choices| choices.as_array()) else {
        return Ok(());
    };
    for choice in choices {
        if let Some(error) = choice.get("error") {
            return Err(format!(
                "chat completions stream choice error: {}",
                provider_error_summary(error)
            ));
        }
        capture_chat_finish_reason(choice, state);
        if let Some(delta) = choice.get("delta") {
            capture_chat_finish_reason(delta, state);
            if let Some(content) = extract_chat_content_text(delta.get("content")) {
                if !content.is_empty() {
                    state.text.push_str(&content);
                    stream_recorder.record_text_delta(&content);
                    observer.on_event(AgentEvent::TextDelta(&content));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|value| value.as_array()) {
                merge_chat_tool_call_deltas(tool_calls, pending_calls, observer, stream_recorder);
            }
        }

        if let Some(message) = choice.get("message") {
            if state.text.is_empty() {
                if let Some(content) = extract_chat_content_text(message.get("content")) {
                    if !content.is_empty() {
                        state.text.push_str(&content);
                        stream_recorder.record_text_delta(&content);
                        observer.on_event(AgentEvent::TextDelta(&content));
                    }
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|value| value.as_array()) {
                merge_chat_tool_call_deltas(tool_calls, pending_calls, observer, stream_recorder);
            }
        }
    }
    Ok(())
}

fn capture_chat_finish_reason(choice_or_delta: &Value, state: &mut ChatStreamTurn) {
    if let Some(reason) = choice_or_delta
        .get("finish_reason")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        state.finish_reason = Some(reason.to_string());
    }
    if let Some(reason) = choice_or_delta
        .get("native_finish_reason")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        state.native_finish_reason = Some(reason.to_string());
    }
}

fn extract_chat_content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    match content {
        Value::String(text) => {
            if text.is_empty() {
                None
            } else {
                Some(text.clone())
            }
        }
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(piece) = extract_chat_content_part_text(part) {
                    text.push_str(&piece);
                }
            }
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Value::Object(_) => extract_chat_content_part_text(content),
        _ => None,
    }
}

fn extract_chat_content_part_text(part: &Value) -> Option<String> {
    if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    if let Some(text) = part.get("content").and_then(|value| value.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    if let Some(delta) = part.get("delta").and_then(|value| value.as_str()) {
        if !delta.is_empty() {
            return Some(delta.to_string());
        }
    }
    None
}

fn merge_chat_tool_call_deltas(
    tool_calls: &[Value],
    pending_calls: &mut HashMap<usize, PendingCall>,
    observer: &mut dyn AgentObserver,
    stream_recorder: &mut dyn StreamCheckpointSink,
) {
    for tool_call in tool_calls {
        let index = tool_call
            .get("index")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize;
        let pending = pending_calls.entry(index).or_insert_with(|| PendingCall {
            call_id: format!("call_{index}"),
            name: String::new(),
            arguments: String::new(),
        });
        if let Some(id) = tool_call.get("id").and_then(|value| value.as_str()) {
            if !id.is_empty() {
                pending.call_id = id.to_string();
            }
        }
        if let Some(function) = tool_call.get("function") {
            if let Some(name) = function.get("name").and_then(|value| value.as_str()) {
                if !name.is_empty() {
                    pending.name = name.to_string();
                }
            }
            let arguments = function
                .get("arguments")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if !arguments.is_empty() {
                pending.arguments.push_str(arguments);
            }
            if !pending.name.is_empty() || !arguments.is_empty() {
                observer.on_event(AgentEvent::ToolCallDelta {
                    call_id: Some(&pending.call_id),
                    name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    arguments_delta: arguments,
                });
                stream_recorder.record_tool_delta(
                    Some(&pending.call_id),
                    (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    arguments,
                );
            }
        }
    }
}

fn finalize_chat_pending_calls(
    state: &mut ChatStreamTurn,
    pending_calls: HashMap<usize, PendingCall>,
) {
    let mut calls = pending_calls.into_iter().collect::<Vec<_>>();
    calls.sort_by_key(|(index, _)| *index);
    for (_index, pending) in calls {
        if pending.name.is_empty() {
            continue;
        }
        let arguments = if pending.arguments.trim().is_empty() {
            json!({})
        } else {
            parse_tool_arguments(&pending.arguments)
        };
        if state
            .tool_calls
            .iter()
            .any(|existing| existing.call_id == pending.call_id)
        {
            continue;
        }
        state.tool_calls.push(ToolCall {
            call_id: pending.call_id,
            name: pending.name,
            arguments,
        });
    }
}

fn upstream_status_error(endpoint: &str, status: reqwest::StatusCode, body: &str) -> String {
    let body = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return endpoint_error_message(endpoint, status.as_u16(), &value, "login required");
    }

    if body.is_empty() {
        format!("{endpoint} returned {status}")
    } else {
        format!("{endpoint} returned {status}: {body}")
    }
}

fn empty_stream_error(endpoint: &str) -> String {
    format!("{endpoint} returned an empty response without text or tool calls")
}

fn empty_stream_after_tools_error(endpoint: &str, summary: &str) -> String {
    format!(
        "{endpoint} returned an empty response without a final text response after tool execution.\n\nTool summary:\n{summary}"
    )
}

fn chat_empty_turn_error(endpoint: &str, turn: &ChatStreamTurn) -> String {
    let finish = turn
        .finish_reason
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown");
    let native = turn
        .native_finish_reason
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("; native_finish_reason={value}"))
        .unwrap_or_default();
    format!("{endpoint} returned no visible text; finish_reason={finish}{native}")
}

fn provider_error_summary(error: &Value) -> String {
    if let Some(message) = error.get("message").and_then(|value| value.as_str()) {
        let code = error
            .get("code")
            .map(|value| format!("code={value}; "))
            .unwrap_or_default();
        let metadata = error
            .get("metadata")
            .map(|value| format!("; metadata={}", preview_text(&value.to_string(), 500)))
            .unwrap_or_default();
        return format!("{code}{message}{metadata}");
    }
    preview_text(&error.to_string(), 800)
}

fn stream_buffer_preview(buffer: &str) -> String {
    if buffer.trim().is_empty() {
        String::new()
    } else {
        format!("; buffered_sse_tail={}", preview_text(buffer, 500))
    }
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let mut preview = text
        .chars()
        .take(max_chars)
        .collect::<String>()
        .replace('\n', "\\n");
    if text.chars().count() > max_chars {
        preview.push_str("...");
    }
    preview
}

#[derive(Debug, Clone)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn handle_stream_event(
    event: &Value,
    state: &mut StreamTurn,
    pending_calls: &mut HashMap<String, PendingCall>,
    observer: &mut dyn AgentObserver,
    stream_recorder: &mut dyn StreamCheckpointSink,
) -> Result<(), String> {
    let event_type = event
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default();

    match event_type {
        "response.created" | "response.in_progress" | "response.queued" => {}
        "response.output_text.delta" => {
            let delta = event
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !delta.is_empty() {
                state.text.push_str(delta);
                stream_recorder.record_text_delta(delta);
                observer.on_event(AgentEvent::TextDelta(delta));
            }
        }
        "response.output_item.added" => {
            if let Some(item) = event.get("item") {
                if let Some(pending) = parse_pending_call(item) {
                    observer.on_event(AgentEvent::ToolCallDelta {
                        call_id: Some(&pending.call_id),
                        name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        arguments_delta: &pending.arguments,
                    });
                    stream_recorder.record_tool_delta(
                        Some(&pending.call_id),
                        (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        &pending.arguments,
                    );
                    pending_calls.insert(pending_key(item), pending);
                } else if let Some(text) = extract_message_text(item) {
                    if !text.is_empty() && state.text.is_empty() {
                        state.text.push_str(&text);
                        observer.on_event(AgentEvent::TextDelta(&text));
                    }
                }
            }
        }
        "response.output_text.done" => {
            if state.text.is_empty() {
                if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                    state.text.push_str(text);
                } else if let Some(output_text) = event.get("output_text").and_then(|v| v.as_str())
                {
                    state.text.push_str(output_text);
                }
            }
        }
        "response.content_part.done" => {
            if state.text.is_empty() {
                if let Some(part) = event.get("part") {
                    if let Some(text) = extract_message_text(part) {
                        state.text.push_str(&text);
                    }
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = event
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let delta = event
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !item_id.is_empty() && !delta.is_empty() {
                let pending =
                    pending_calls
                        .entry(item_id.to_string())
                        .or_insert_with(|| PendingCall {
                            call_id: item_id.to_string(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                pending.arguments.push_str(delta);
                observer.on_event(AgentEvent::ToolCallDelta {
                    call_id: Some(&pending.call_id),
                    name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    arguments_delta: delta,
                });
                stream_recorder.record_tool_delta(
                    Some(&pending.call_id),
                    (!pending.name.is_empty()).then_some(pending.name.as_str()),
                    delta,
                );
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = event
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let name = event
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let arguments = event
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !item_id.is_empty() {
                let pending =
                    pending_calls
                        .entry(item_id.to_string())
                        .or_insert_with(|| PendingCall {
                            call_id: item_id.to_string(),
                            name: name.to_string(),
                            arguments: String::new(),
                        });
                if pending.name.is_empty() && !name.is_empty() {
                    pending.name = name.to_string();
                }
                if pending.arguments.is_empty() && !arguments.is_empty() {
                    pending.arguments = arguments.to_string();
                }
                if !pending.name.is_empty() || !arguments.is_empty() {
                    observer.on_event(AgentEvent::ToolCallDelta {
                        call_id: Some(&pending.call_id),
                        name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        arguments_delta: arguments,
                    });
                    stream_recorder.record_tool_delta(
                        Some(&pending.call_id),
                        (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        arguments,
                    );
                }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                if let Some(pending) = parse_pending_call(item) {
                    observer.on_event(AgentEvent::ToolCallDelta {
                        call_id: Some(&pending.call_id),
                        name: (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        arguments_delta: &pending.arguments,
                    });
                    stream_recorder.record_tool_delta(
                        Some(&pending.call_id),
                        (!pending.name.is_empty()).then_some(pending.name.as_str()),
                        &pending.arguments,
                    );
                    pending_calls.insert(pending_key(item), pending);
                } else if let Some(text) = extract_message_text(item) {
                    if state.text.is_empty() {
                        state.text.push_str(&text);
                    }
                }
            }
        }
        "response.completed" => {
            if let Some(response) = event.get("response") {
                if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                    state.response_id = Some(id.to_string());
                }
                state.usage = parse_usage(response);
                let turn = parse_turn(response.clone())?;
                for call in turn.tool_calls {
                    if !state
                        .tool_calls
                        .iter()
                        .any(|existing| existing.call_id == call.call_id)
                    {
                        state.tool_calls.push(call);
                    }
                }
                if state.text.is_empty() {
                    if let Some(text) = turn.text.or_else(|| extract_text(response)) {
                        state.text = text;
                    }
                }
            }

            for pending in pending_calls.values() {
                if pending.name.is_empty() {
                    continue;
                }
                if state
                    .tool_calls
                    .iter()
                    .any(|existing| existing.call_id == pending.call_id)
                {
                    continue;
                }
                let arguments = if pending.arguments.trim().is_empty() {
                    json!({})
                } else {
                    parse_tool_arguments(&pending.arguments)
                };
                state.tool_calls.push(ToolCall {
                    call_id: pending.call_id.clone(),
                    name: pending.name.clone(),
                    arguments,
                });
            }
        }
        "response.failed" | "response.error" | "response.incomplete" | "error" => {
            return Err(format!("responses stream reported {event_type}: {event}"));
        }
        _ => {}
    }

    Ok(())
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

fn extract_sse_event(event_block: &str) -> Option<(Option<String>, String)> {
    let mut event_name = None;
    let mut data = Vec::new();
    for line in event_block.lines() {
        if let Some(name) = line.strip_prefix("event:") {
            let name = name.trim();
            if !name.is_empty() {
                event_name = Some(name.to_string());
            }
            continue;
        }
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start().to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some((event_name, data.join("\n")))
    }
}

fn attach_sse_event_type(event: &mut Value, event_name: Option<&str>) {
    let Some(event_name) = event_name else {
        return;
    };
    if event.get("type").and_then(|v| v.as_str()).is_some() {
        return;
    }
    if let Value::Object(map) = event {
        map.insert("type".to_string(), Value::String(event_name.to_string()));
    }
}

fn parse_pending_call(item: &Value) -> Option<PendingCall> {
    let item_type = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if item_type != "function_call" {
        return None;
    }

    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let name = item.get("name").and_then(|v| v.as_str())?.to_string();
    let arguments = item
        .get("arguments")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Some(PendingCall {
        call_id,
        name,
        arguments,
    })
}

fn pending_key(item: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("call_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

#[allow(dead_code)]
fn parse_turn(value: Value) -> Result<TurnResult, String> {
    let response_id = value
        .get("id")
        .and_then(|id| id.as_str())
        .map(|id| id.to_string());

    let mut text = extract_text(&value);
    let mut tool_calls = Vec::new();

    if let Some(output) = value.get("output").and_then(|output| output.as_array()) {
        for item in output {
            if let Some(item_type) = item.get("type").and_then(|v| v.as_str()) {
                match item_type {
                    "message" => {
                        if text.is_none() {
                            text = extract_message_text(item);
                        }
                    }
                    "function_call" | "tool_call" | "custom_tool_call" => {
                        if let Some(call) = parse_tool_call(item)? {
                            tool_calls.push(call);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(TurnResult {
        response_id,
        text,
        tool_calls,
    })
}

fn extract_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(|v| v.as_str()) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Some(output) = value.get("output").and_then(|output| output.as_array()) {
        for item in output {
            if let Some(text) = extract_message_text(item) {
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }

    None
}

fn extract_message_text(item: &Value) -> Option<String> {
    let content = item.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for entry in content {
        match entry.get("type").and_then(|v| v.as_str()) {
            Some("output_text") | Some("text") => {
                if let Some(text) = entry.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

#[allow(dead_code)]
fn parse_tool_call(item: &Value) -> Result<Option<ToolCall>, String> {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if name.is_empty() || call_id.is_empty() {
        return Ok(None);
    }

    let arguments = match item.get("arguments") {
        Some(Value::String(raw)) => parse_tool_arguments(raw),
        Some(value) => value.clone(),
        None => json!({}),
    };

    Ok(Some(ToolCall {
        call_id,
        name,
        arguments,
    }))
}

fn parse_tool_arguments(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }

    let mut candidates = Vec::new();
    if trimmed.starts_with('{') && !trimmed.ends_with('}') {
        candidates.push(format!("{trimmed}}}"));
    }
    if !trimmed.starts_with('{') && trimmed.contains(':') {
        candidates.push(format!("{{{trimmed}}}"));
    }
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
            return value;
        }
    }

    json!({ "raw": raw })
}

fn dispatch_tool(
    paths: &AppPaths,
    _config: &AppConfig,
    toolbox: &BoxTools<'_>,
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    call: ToolCall,
) -> Result<ToolDispatchResult, String> {
    match call.name.as_str() {
        name if name.starts_with("mcp__") => dispatch_mcp_tool(mcp_registry, mcp_tools, call),
        "mcp_list" => {
            let text = render_mcp_inventory(mcp_registry, mcp_tools);
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "git" => {
            let command = extract_string_array(&call.arguments, "command")?;
            let reason = optional_string(&call.arguments, "reason");
            toolbox
                .git(box_id, &command, reason.as_deref())
                .map(|response| {
                    let mut text = String::from("Git\n```bash\n");
                    text.push_str(&command.join(" "));
                    text.push_str("\n```\n\n");
                    text.push_str(&response.text);
                    if command
                        .first()
                        .map(|part| part == "commit")
                        .unwrap_or(false)
                    {
                        let hooks = HookStore::new(paths).ok();
                        if let Some(hooks) = hooks {
                            let _ = hooks.run_event(toolbox.sandbox(), box_id, "after_commit");
                        }
                    }
                    ToolDispatchResult {
                        model_output: text.clone(),
                        ui_output: text,
                    }
                })
        }
        "gh" => {
            let command = extract_string_array(&call.arguments, "command")?;
            let reason = optional_string(&call.arguments, "reason");
            toolbox
                .gh(box_id, &command, reason.as_deref())
                .map(|response| {
                    let mut text = String::from("GH\n```bash\n");
                    text.push_str(&command.join(" "));
                    text.push_str("\n```\n\n");
                    text.push_str(&response.text);
                    ToolDispatchResult {
                        model_output: text.clone(),
                        ui_output: text,
                    }
                })
        }
        "update_plan" => {
            let explanation = call
                .arguments
                .get("explanation")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let plan = call
                .arguments
                .get("plan")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let items = plan
                .iter()
                .map(|item| {
                    let step = item
                        .get("step")
                        .and_then(|v| v.as_str())
                        .unwrap_or("step")
                        .to_string();
                    let status = item
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("pending")
                        .to_string();
                    (step, status)
                })
                .collect::<Vec<_>>();
            let response = toolbox.update_plan(explanation, &items);
            let model_output = plan_model_output(&response.text, &items);
            Ok(ToolDispatchResult {
                model_output,
                ui_output: response.text,
            })
        }
        "plan" => {
            let goal = call
                .arguments
                .get("goal")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let steps = call
                .arguments
                .get("steps")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let items = steps
                .iter()
                .map(|item| {
                    let step = item
                        .get("step")
                        .and_then(|v| v.as_str())
                        .unwrap_or("step")
                        .to_string();
                    let status = item
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("pending")
                        .to_string();
                    (step, status)
                })
                .collect::<Vec<_>>();
            let response = toolbox.plan(goal, &items);
            let model_output = plan_model_output(&response.text, &items);
            Ok(ToolDispatchResult {
                model_output,
                ui_output: response.text,
            })
        }
        "subagent" => {
            let role_value = extract_string(&call.arguments, "role")?;
            let role = SubagentRole::from_value(&role_value).ok_or_else(|| {
                format!(
                    "unknown subagent role `{role_value}`; use planner, codebase_researcher, patcher, reviewer, test_runner, or security_auditor"
                )
            })?;
            let task = extract_string(&call.arguments, "task")?;
            let paths = optional_string_array(&call.arguments, "paths");
            let report = run_subagent(toolbox.sandbox(), box_id, role, &task, &paths);
            let text = report.to_model_output();
            append_agent_checkpoint(
                store,
                project_key,
                session_id,
                "subagent_run",
                json!({
                    "role": report.role.as_str(),
                    "status": report.status.as_str(),
                    "task": task,
                    "commands": report.commands.iter().map(|command| json!({
                        "label": command.label.clone(),
                        "command": command.command.clone(),
                        "status": command.status.as_str(),
                        "exit_code": command.exit_code,
                    })).collect::<Vec<_>>(),
                }),
            )?;
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "review" => {
            let questions = call
                .arguments
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .take(10)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let response = toolbox.review(&questions);
            Ok(ToolDispatchResult {
                model_output: format!(
                    "{}\nUser answers will arrive as normal queued prompts when provided.",
                    response.text
                ),
                ui_output: response.text,
            })
        }
        "hook" => {
            let action = call
                .arguments
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("list")
                .trim()
                .to_ascii_lowercase();
            let hooks = HookStore::new(paths)?;
            match action.as_str() {
                "list" => {
                    let records = hooks.list()?;
                    let response = toolbox.hook_summary(
                        "list", None, None, None, None, None, None, None, None, &records,
                    );
                    Ok(ToolDispatchResult {
                        model_output: response.text.clone(),
                        ui_output: response.text,
                    })
                }
                "remove" => {
                    let hook_id = extract_string(&call.arguments, "id")?;
                    if !hooks.remove(&hook_id)? {
                        return Err(format!("hook not found: {hook_id}"));
                    }
                    let response = toolbox.hook_summary(
                        "remove",
                        Some(&hook_id),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &[],
                    );
                    Ok(ToolDispatchResult {
                        model_output: response.text.clone(),
                        ui_output: response.text,
                    })
                }
                "add" | "create" => {
                    let event = call
                        .arguments
                        .get("event")
                        .and_then(|v| v.as_str())
                        .unwrap_or("after_shell")
                        .trim()
                        .to_string();
                    let command = extract_string_array(&call.arguments, "command")?;
                    let match_command = call
                        .arguments
                        .get("match_command")
                        .and_then(|v| v.as_str())
                        .map(|value| value.to_string());
                    let match_tool = call
                        .arguments
                        .get("match_tool")
                        .and_then(|v| v.as_str())
                        .map(|value| value.to_string());
                    let match_status = call
                        .arguments
                        .get("match_status")
                        .and_then(|v| v.as_str())
                        .map(|value| value.to_string());
                    let match_path = call
                        .arguments
                        .get("match_path")
                        .and_then(|v| v.as_str())
                        .map(|value| value.to_string());
                    let match_mode = crate::hooks::HookMatchMode::from_value(
                        call.arguments.get("match_mode").and_then(|v| v.as_str()),
                    );
                    let record = hooks.add_scoped(
                        &event,
                        command.clone(),
                        match_command.clone(),
                        match_mode,
                        match_tool.clone(),
                        match_status.clone(),
                        match_path.clone(),
                    )?;
                    let response = toolbox.hook_summary(
                        "add",
                        Some(&record.id),
                        Some(&record.event),
                        Some(&record.command),
                        record.match_command.as_deref(),
                        record.match_mode.as_deref(),
                        record.match_tool.as_deref(),
                        record.match_status.as_deref(),
                        record.match_path.as_deref(),
                        &[],
                    );
                    Ok(ToolDispatchResult {
                        model_output: response.text.clone(),
                        ui_output: response.text,
                    })
                }
                other => Err(format!("unsupported hook action: {other}")),
            }
        }
        "shell" => {
            let command = extract_string_array(&call.arguments, "command")?;
            let reason = optional_string(&call.arguments, "reason");
            toolbox
                .shell(box_id, &command, reason.as_deref())
                .map(|response| {
                    let mut text = String::from("Executed\n```bash\n");
                    text.push_str(&command.join(" "));
                    text.push_str("\n```\n\n");
                    text.push_str(&response.text);
                    if let Ok(hooks) = HookStore::new(paths) {
                        if let Ok(executions) = hooks.run_event_with_command(
                            toolbox.sandbox(),
                            box_id,
                            "after_shell",
                            Some(&command),
                        ) {
                            append_hook_executions(&mut text, &executions);
                        }
                    }
                    ToolDispatchResult {
                        model_output: text.clone(),
                        ui_output: text,
                    }
                })
        }
        "apply_patch" => {
            let patch_text = extract_string(&call.arguments, "patch_text")?;
            toolbox.apply_patch(box_id, &patch_text).map(|response| {
                let mut text = summarize_patch_text(&patch_text);
                if !response.text.trim().is_empty() {
                    text.push_str("\n\n");
                    text.push_str(&response.text);
                }
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "navigate" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.navigate(box_id, &path).map(|response| {
                let text = response.text;
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "list_dir" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.list_dir(box_id, &path).map(|response| {
                let mut text = String::from("Listed\n");
                text.push_str(&response.text);
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "read_file" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.read_file(box_id, &path).map(|response| {
                let ui_output = if response.text.trim().is_empty() {
                    String::from("Empty File")
                } else {
                    let mut text = String::from("Read ");
                    text.push_str(&path);
                    text
                };
                let model_output = if response.text.trim().is_empty() {
                    String::from("Empty File")
                } else {
                    response.text
                };
                ToolDispatchResult {
                    model_output,
                    ui_output,
                }
            })
        }
        "write_file" => {
            let path = extract_string(&call.arguments, "path")?;
            let content = extract_string(&call.arguments, "content")?;
            toolbox.write_file(box_id, &path, &content).map(|response| {
                let mut text = String::from("Wrote\n");
                text.push_str(&path);
                text.push_str("\n\n");
                text.push_str(&response.text);
                ToolDispatchResult {
                    model_output: format!("Wrote\n{path}\nwritten"),
                    ui_output: text,
                }
            })
        }
        "read_lines" => {
            let path = extract_string(&call.arguments, "path")?;
            let start_line = optional_usize(&call.arguments, "start_line")
                .or_else(|| optional_usize(&call.arguments, "line"))
                .unwrap_or(1)
                .max(1);
            let end_line = optional_usize(&call.arguments, "end_line")
                .or_else(|| optional_usize(&call.arguments, "end"))
                .or_else(|| {
                    optional_usize(&call.arguments, "count")
                        .map(|count| start_line.saturating_add(count.max(1)).saturating_sub(1))
                })
                .unwrap_or_else(|| start_line.saturating_add(120));
            toolbox
                .read_lines(box_id, &path, start_line, end_line)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "grep_lines" => {
            let path = extract_string(&call.arguments, "path")?;
            let pattern = extract_string(&call.arguments, "pattern")?;
            let before = extract_usize(&call.arguments, "before").unwrap_or(2);
            let after = extract_usize(&call.arguments, "after").unwrap_or(2);
            let literal = call
                .arguments
                .get("literal")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let max_matches = extract_usize(&call.arguments, "max_matches").unwrap_or(20);
            toolbox
                .grep_lines(box_id, &path, &pattern, before, after, literal, max_matches)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "head_lines" => {
            let path = extract_string(&call.arguments, "path")?;
            let count = extract_usize(&call.arguments, "count")?;
            toolbox
                .head_lines(box_id, &path, count)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "tail_lines" => {
            let path = extract_string(&call.arguments, "path")?;
            let count = extract_usize(&call.arguments, "count")?;
            toolbox
                .tail_lines(box_id, &path, count)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "glob_files" => {
            let pattern = extract_string(&call.arguments, "pattern")?;
            let max_items = extract_usize(&call.arguments, "max_items").unwrap_or(50);
            toolbox
                .glob_files(box_id, &pattern, max_items)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "replace_in_file" => {
            let path = extract_string(&call.arguments, "path")?;
            let find = extract_string(&call.arguments, "find")?;
            let replace = extract_string(&call.arguments, "replace")?;
            let all = call
                .arguments
                .get("all")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            toolbox
                .replace_in_file(box_id, &path, &find, &replace, all)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "delete_file" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox
                .delete_file(box_id, &path)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "copy_file" => {
            let source_path = extract_string(&call.arguments, "source_path")?;
            let destination_path = extract_string(&call.arguments, "destination_path")?;
            toolbox
                .copy_file(box_id, &source_path, &destination_path)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "move_file" => {
            let source_path = extract_string(&call.arguments, "source_path")?;
            let destination_path = extract_string(&call.arguments, "destination_path")?;
            toolbox
                .move_file(box_id, &source_path, &destination_path)
                .map(|response| ToolDispatchResult {
                    model_output: response.text.clone(),
                    ui_output: response.text,
                })
        }
        "session_remember" => {
            let content = redact_secrets(&extract_string(&call.arguments, "content")?);
            let kind = call
                .arguments
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("note")
                .to_string();
            let tags = call
                .arguments
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let record = store
                .remember_session_memory(project_key, session_id, &kind, &content, &tags)
                .map_err(|e| e.to_string())?;
            let memory_context = MemoryContextStore::new(paths)?;
            let _ = memory_context.remember(
                project_key,
                &record.kind,
                &record.content,
                &tags,
                Some(session_id),
                "session",
                0.55,
            )?;
            let mut text = String::from("Saved Session Context\n");
            text.push_str("kind: ");
            text.push_str(&record.kind);
            text.push_str("\n\n");
            text.push_str(&record.content);
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "session_recall" => {
            let query = extract_string(&call.arguments, "query")?;
            let records = store.recall_session_memory(project_key, session_id, &query, 8)?;
            let mut text = String::from("Context Recovered From Current Session\n");
            text.push_str("query: ");
            text.push_str(&redact_secrets(&query));
            if !records.is_empty() {
                text.push_str("\n\n");
                for record in records {
                    text.push_str("- [");
                    text.push_str(&record.kind);
                    text.push_str("] ");
                    text.push_str(&redact_secrets(&record.content));
                    if !record.tags.trim().is_empty() {
                        text.push_str(" (tags: ");
                        text.push_str(&record.tags);
                        text.push(')');
                    }
                    text.push('\n');
                }
            }
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "search" => {
            let pattern = extract_string(&call.arguments, "pattern")?;
            toolbox.search(box_id, &pattern).map(|response| {
                let mut text = String::from("Searched\n");
                text.push_str(&pattern);
                text.push_str("\n\n```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "remember" => {
            let content = redact_secrets(&extract_string(&call.arguments, "content")?);
            let kind = call
                .arguments
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("fact")
                .to_string();
            let importance = call
                .arguments
                .get("importance")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5);
            let confidence = call
                .arguments
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5);
            let tags = call
                .arguments
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let response = toolbox
                .remember(
                    project_key,
                    AnchorInput {
                        kind,
                        content: content.clone(),
                        tags: tags.clone(),
                        importance,
                        confidence,
                        source_session_id: Some(session_id.to_string()),
                    },
                )
                .map_err(|e| e.to_string())?;
            let memory_context = MemoryContextStore::new(paths)?;
            let _ = memory_context.remember(
                project_key,
                "fact",
                &content,
                &tags,
                Some(session_id),
                "anchor",
                importance,
            )?;
            let text = format!("Saved Context\n\n{}", response.text);
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "recall" => {
            let query = extract_string(&call.arguments, "query")?;
            toolbox.recall(project_key, &query).map(|response| {
                let mut text = String::from("Context Recovered From Other Session\n");
                text.push_str("query: ");
                text.push_str(&redact_secrets(&query));
                text.push_str("\n\n```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                ToolDispatchResult {
                    model_output: text.clone(),
                    ui_output: text,
                }
            })
        }
        "lab_learn" => {
            let content = redact_secrets(&extract_string(&call.arguments, "content")?);
            let kind = call
                .arguments
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("preference")
                .to_string();
            let confidence = call
                .arguments
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.6);
            let tags = call
                .arguments
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let lab = LabStore::new(paths)?;
            let record = lab.remember(
                project_key,
                LabInput {
                    kind,
                    content,
                    tags,
                    confidence,
                    source_session_id: Some(session_id.to_string()),
                },
                "tool",
            )?;
            let mut text = String::from("Lab Learned\n");
            text.push_str("kind: ");
            text.push_str(&record.kind);
            text.push_str("\nconfidence: ");
            text.push_str(&format!("{:.2}", record.confidence));
            if !record.tags.is_empty() {
                text.push_str("\ntags: ");
                text.push_str(&record.tags.join(", "));
            }
            text.push_str("\n\n");
            text.push_str(&record.content);
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "lab_recall" => {
            let query = extract_string(&call.arguments, "query")?;
            let lab = LabStore::new(paths)?;
            let records = lab.recall(project_key, &query, 8)?;
            let mut text = String::from("Lab Recall\n");
            text.push_str("query: ");
            text.push_str(&redact_secrets(&query));
            if records.is_empty() {
                text.push_str("\nNo matching Lab preferences found.");
            } else {
                text.push_str("\n\n");
                for record in records {
                    text.push_str("- [");
                    text.push_str(&record.kind);
                    text.push_str("] ");
                    text.push_str(&record.content);
                    if !record.tags.is_empty() {
                        text.push_str(" (tags: ");
                        text.push_str(&record.tags.join(", "));
                        text.push(')');
                    }
                    text.push('\n');
                }
            }
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "skill_list" => {
            let store = SkillStore::new(paths)?;
            let records = store.list()?;
            let mut text = String::from("Skills\n");
            if records.is_empty() {
                text.push_str("No local skills yet.");
            } else {
                for record in records {
                    text.push_str("- ");
                    text.push_str(&record.name);
                    if !record.description.trim().is_empty() {
                        text.push_str(" :: ");
                        text.push_str(&record.description);
                    }
                    text.push('\n');
                }
            }
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "skill_read" => {
            let name = extract_string(&call.arguments, "name")?;
            let store = SkillStore::new(paths)?;
            let record = store.read(&name)?;
            let text = format!(
                "Skill\n{}\n{}\n\n```markdown\n{}\n```",
                record.name,
                record.path.display(),
                record.body
            );
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        "skill_create" => {
            let name = extract_string(&call.arguments, "name")?;
            let description = extract_string(&call.arguments, "description")?;
            let body = extract_string(&call.arguments, "body")?;
            let store = SkillStore::new(paths)?;
            let record = store.create(&name, &description, &body)?;
            let text = format!(
                "Skill Created\n{}\n{}\n\n{}",
                record.name,
                record.path.display(),
                record.description
            );
            Ok(ToolDispatchResult {
                model_output: text.clone(),
                ui_output: text,
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn dispatch_mcp_tool(
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    call: ToolCall,
) -> Result<ToolDispatchResult, String> {
    let spec = mcp_tools
        .iter()
        .find(|tool| tool.function_name == call.name)
        .ok_or_else(|| format!("unknown MCP tool: {}", call.name))?;
    mcp_registry.call_tool(spec, &call.arguments).map(|output| {
        let output = redact_secrets(&output);
        let mut text = String::from("MCP Tool\n");
        text.push_str(&spec.server_name);
        text.push_str("::");
        text.push_str(&spec.tool_name);
        if !output.trim().is_empty() {
            text.push_str("\n\n```text\n");
            text.push_str(&output);
            text.push_str("\n```");
        }
        ToolDispatchResult {
            model_output: text.clone(),
            ui_output: text,
        }
    })
}

fn summarize_patch_text(patch_text: &str) -> String {
    let mut out = String::from("Patched\n");
    let mut current_file = String::new();
    let mut diff_lines = Vec::new();

    for line in patch_text.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush_patch_section(&mut out, &current_file, &diff_lines);
            current_file = path.trim().to_string();
            diff_lines.clear();
            diff_lines.push(format!("+ added file {}", current_file));
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush_patch_section(&mut out, &current_file, &diff_lines);
            current_file = path.trim().to_string();
            diff_lines.clear();
            diff_lines.push(format!("- deleted file {}", current_file));
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush_patch_section(&mut out, &current_file, &diff_lines);
            current_file = path.trim().to_string();
            diff_lines.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            diff_lines.push(format!("+{}", rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            diff_lines.push(format!("-{}", rest));
            continue;
        }
        if line.starts_with(" ") || line.starts_with("@@") || line.trim() == "*** End of File" {
            continue;
        }
    }

    flush_patch_section(&mut out, &current_file, &diff_lines);
    out
}

fn flush_patch_section(out: &mut String, file: &str, diff_lines: &[String]) {
    if file.is_empty() {
        return;
    }

    out.push_str("\nFile: ");
    out.push_str(file);
    out.push_str("\n```diff\n");
    if diff_lines.is_empty() {
        out.push_str("(no inline diff lines captured)\n");
    } else {
        for line in diff_lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("```\n");
}

fn append_hook_executions(text: &mut String, executions: &[crate::hooks::HookExecution]) {
    if executions.is_empty() {
        return;
    }
    text.push_str("\n\nTriggered hooks\n");
    for execution in executions {
        text.push_str("- ");
        text.push_str(&execution.id);
        text.push_str(" :: ");
        text.push_str(&execution.event);
        text.push_str(" => ");
        text.push_str(&execution.command.join(" "));
        if !execution.output.trim().is_empty() {
            text.push_str("\n```text\n");
            text.push_str(&execution.output);
            text.push_str("\n```\n");
        } else {
            text.push('\n');
        }
    }
}

fn extract_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field: {key}"))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn optional_string_array(value: &Value, key: &str) -> Vec<String> {
    if let Some(array) = value.get(key).and_then(|v| v.as_array()) {
        return array
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| item.to_string())
            .collect();
    }
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| vec![item.to_string()])
        .unwrap_or_default()
}

fn extract_string_array(value: &Value, key: &str) -> Result<Vec<String>, String> {
    if let Some(array) = value.get(key).and_then(|v| v.as_array()) {
        let mut values = Vec::new();
        for entry in array {
            let item = entry
                .as_str()
                .ok_or_else(|| format!("field {key} must contain strings"))?;
            values.push(item.to_string());
        }
        return Ok(values);
    }

    if let Some(command) = value.get(key).and_then(|v| v.as_str()) {
        return split_command_line(command)
            .map_err(|err| format!("invalid command field {key}: {err}"));
    }

    Err(format!("missing array field: {key}"))
}

fn extract_usize(value: &Value, key: &str) -> Result<usize, String> {
    optional_usize(value, key).ok_or_else(|| format!("missing numeric field: {key}"))
}

fn optional_usize(value: &Value, key: &str) -> Option<usize> {
    let raw = value.get(key)?;
    if let Some(number) = raw.as_u64() {
        return Some(number as usize);
    }
    if let Some(number) = raw.as_i64() {
        return usize::try_from(number).ok();
    }
    if let Some(number) = raw.as_f64() {
        if number.is_finite() && number >= 0.0 {
            return Some(number.round() as usize);
        }
    }
    raw.as_str()
        .and_then(|value| value.trim().parse::<usize>().ok())
}

fn tool_definitions(paths: &AppPaths, mcp_tools: &[McpToolSpec]) -> Value {
    let mut tools = vec![
        json!({
            "type": "function",
            "name": "shell",
            "description": "Run a project-local command inside the Box workspace. Pass argv directly; do not use bash -c, sh -c, or shell wrapper commands. Regex/metacharacters inside argv literals are allowed, but do not build shell pipelines or redirections as command strings. Prefer search, grep_lines, read_file, read_lines, and glob_files for inspection. Use shell for installs, builds, tests, formatters, linters, code generation, and dev commands that stay inside the project tree.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Program and arguments to execute from the current Box directory. Examples: [\"pnpm\",\"install\"], [\"pnpm\",\"build\"], [\"cargo\",\"test\"], [\"rg\",\"-n\",\"loop\\\\|turn\",\"src\"]. Do not include sudo, shell wrappers, or absolute paths outside the project. /workspace/... and host absolute project paths are allowed inside the project."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One sentence explaining why this command is necessary. Guardian mode sends this to the command reviewer."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "git",
            "description": "Run git commands inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Git subcommand and arguments to execute."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One sentence explaining why this git command is necessary."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "gh",
            "description": "Run GitHub CLI commands inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "GitHub CLI subcommand and arguments to execute."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One sentence explaining why this GitHub CLI command is necessary."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "update_plan",
            "description": "Create or update the visible implementation plan. Use this for multi-step work, with exactly one in_progress item when work is underway.",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Short reason for the current plan update."
                    },
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "Concrete task step."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current step status."
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "plan",
            "description": "Create a visible plan for non-trivial work before executing it. Use this by default for broad, risky, architectural, migration, debugging, or multi-step tasks. Do not use it for one-shot inspections or tiny edits. Use update_plan to keep the plan current as work progresses.",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Concrete objective for this run."
                    },
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "Specific action to perform."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Step status. Exactly one step should be in_progress when work starts."
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["goal", "steps"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "subagent",
            "description": "Run a specialized Wire subagent and return its report to the main agent. Subagents never expand permissions: they are scoped analysis workers with no network and no direct file edits. Use them for planner, codebase_researcher, patcher, reviewer, test_runner, and security_auditor roles. Command denials or tool failures are report evidence; continue with a different route instead of stopping.",
            "parameters": {
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["planner", "codebase_researcher", "patcher", "reviewer", "test_runner", "security_auditor"],
                        "description": "Specialized role to run."
                    },
                    "task": {
                        "type": "string",
                        "description": "Focused objective for the subagent."
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional edited or relevant paths for roles such as test_runner."
                    }
                },
                "required": ["role", "task"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "hook",
            "description": "Manage automation hooks stored in `~/.wirecli/hooks.json`. Use this to create, list, or remove automatic follow-up commands after agent actions. Keep hooks project-scoped, narrowly matched, explicit, and free of secrets. Prefer exact command matching for safety.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "create", "remove"],
                        "description": "Hook operation."
                    },
                    "event": {
                        "type": "string",
                        "description": "Trigger event such as `session_start`, `pre_tool_use`, `post_tool_use`, `file_changed`, `pre_compact`, `stop`, `stop_failure`, `permission_request`, `after_shell`, `after_edit`, or `after_commit`."
                    },
                    "match_command": {
                        "type": "string",
                        "description": "Optional matcher for the executed command that triggered the event, for example `npm start`."
                    },
                    "match_tool": {
                        "type": "string",
                        "description": "Optional exact tool-name filter, for example `apply_patch`, `shell`, or `subagent`."
                    },
                    "match_status": {
                        "type": "string",
                        "description": "Optional exact lifecycle status filter such as `pending`, `ok`, `failed`, `blocked`, `new`, `resumed`, or `error`."
                    },
                    "match_path": {
                        "type": "string",
                        "description": "Optional path filter for file_changed/pre/post tool events. Uses match_mode for starts_with or contains matching."
                    },
                    "match_mode": {
                        "type": "string",
                        "enum": ["exact", "starts_with", "contains"],
                        "description": "How to match `match_command` and `match_path`. Prefer `exact` unless broader matching is truly required."
                    },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command to run automatically when the hook fires."
                    },
                    "id": {
                        "type": "string",
                        "description": "Hook id to remove."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "review",
            "description": "Ask the user focused review questions when missing information blocks correct execution. Ask at most 10 questions. Use only when repository inspection cannot resolve the ambiguity.",
            "parameters": {
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "maxItems": 10,
                        "items": { "type": "string" },
                        "description": "Concrete questions for the user."
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply a complete structured Wire patch to files inside the Box. Prefer this for modifying existing files with small targeted hunks that preserve unrelated edits. Do not delete/recreate or rewrite existing files when a patch can express the change. Use write_file for new generated files, large generated rewrites, or rare cases where exact patch grammar is genuinely unsuitable.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch_text": {
                        "type": "string",
                        "description": "Complete patch text. It must start with `*** Begin Patch`, contain valid Add/Update/Delete File hunks, and end with `*** End Patch`."
                    }
                },
                "required": ["patch_text"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "list_dir",
            "description": "List folders and files at a path relative to the current Box directory. Output is grouped as Folders and Files. Use navigate to change the current directory instead of manually repeating long paths.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to inspect from the current directory. Use \".\" for current directory."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "navigate",
            "description": "Change the current Box directory. Use this to go into folders or back with \"..\". Navigation cannot escape the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to the current directory. Use \"..\" to go up and \".\" to stay."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "read_file",
            "description": "Read exact file contents inside the Box workspace, relative to the current directory. Use this instead of shell commands like cat, head, tail, sed, grep, or awk when inspecting files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative file path."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "write_file",
            "description": "Create or replace a full file inside the Box workspace. Prefer this for generated files and large rewrites; Wire will show the resulting diff.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative file path."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file contents."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "search",
            "description": "Search the Box workspace for code and text matches.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "read_lines",
            "description": "Read a line range from a file inside the Box workspace with line numbers. Use this instead of loading a huge file when you only need a segment.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First 1-based line to include."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Last 1-based line to include. Optional; Wire reads a safe focused window when omitted."
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional number of lines to read when end_line is omitted."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "grep_lines",
            "description": "Search a specific file or path for matching lines and surrounding context. Use this for line-focused inspection of large files instead of shell grep. Paths may be project-relative, current-directory-relative, or /workspace/... inside the Box.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "pattern": { "type": "string" },
                    "before": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of context lines before each match."
                    },
                    "after": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of context lines after each match."
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat the pattern as a fixed string instead of regex."
                    },
                    "max_matches": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of matches to return."
                    }
                },
                "required": ["path", "pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "head_lines",
            "description": "Read the first N lines of a file with line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Number of lines to read from the top."
                    }
                },
                "required": ["path", "count"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "tail_lines",
            "description": "Read the last N lines of a file with line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Number of lines to read from the bottom."
                    }
                },
                "required": ["path", "count"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "glob_files",
            "description": "List files in the Box workspace that match a glob pattern.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "max_items": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of matched paths to return."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "replace_in_file",
            "description": "Replace text in a file. Use this for precise string substitutions when a patch is unnecessary.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "find": { "type": "string" },
                    "replace": { "type": "string" },
                    "all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of only the first."
                    }
                },
                "required": ["path", "find", "replace"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "delete_file",
            "description": "Delete a file or directory inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to delete." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "copy_file",
            "description": "Copy a file from source to destination inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source_path": { "type": "string" },
                    "destination_path": { "type": "string" }
                },
                "required": ["source_path", "destination_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "move_file",
            "description": "Move or rename a file inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source_path": { "type": "string" },
                    "destination_path": { "type": "string" }
                },
                "required": ["source_path", "destination_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "remember",
            "description": "Store durable Anchor memory for later sessions. Never store secrets, credentials, tokens, or raw keys.",
            "parameters": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Memory kind such as fact, preference, decision, or reminder."
                    },
                    "content": {
                        "type": "string",
                        "description": "The memory content to keep."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags."
                    },
                    "importance": {
                        "type": "number",
                        "description": "Importance score from 0 to 1."
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Confidence score from 0 to 1."
                    }
                },
                "required": ["content"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "recall",
            "description": "Search Anchor memory for relevant prior facts.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search text for the memory store."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "lab_learn",
            "description": "Store a learned user preference, style signal, security concern, UI preference, library preference, or workflow pattern in Lab so Wire can adapt future work. Never store secrets, credentials, tokens, private keys, or raw personal data.",
            "parameters": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Learning kind such as preference, style, security, workflow, frontend, library, or coding."
                    },
                    "content": {
                        "type": "string",
                        "description": "Concise learned preference or pattern."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags."
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Confidence from 0 to 1. Use lower confidence for inferred patterns and higher confidence for explicit user statements."
                    }
                },
                "required": ["content"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "lab_recall",
            "description": "Search Lab for learned user preferences and adaptation guidance relevant to the current task.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search text for Lab preferences."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "session_remember",
            "description": "Store temporary memory for the current chat session. Never store secrets, credentials, tokens, or raw keys.",
            "parameters": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Memory kind such as note, task, state, or reminder."
                    },
                    "content": {
                        "type": "string",
                        "description": "The temporary memory content to keep for this session."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags."
                    }
                },
                "required": ["content"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "session_recall",
            "description": "Search temporary memory for the current chat session.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search text for the session memory store."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "skill_list",
            "description": "List local Wire skills stored as SKILL.md files. Skills capture repeatable workflows and official-doc-backed procedures the agent can reuse.",
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "skill_read",
            "description": "Read a local Wire skill by name before applying a repeatable workflow.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "skill_create",
            "description": "Create or replace a local Wire skill as a SKILL.md file with YAML frontmatter. Use this for repeatable workflows grounded in inspected repo behavior or official documentation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short stable skill name using words like openai-responses or rust-tui-review."
                    },
                    "description": {
                        "type": "string",
                        "description": "One sentence describing when this skill should be used."
                    },
                    "body": {
                        "type": "string",
                        "description": "Markdown instructions. Keep it operational, scoped, and free of secrets."
                    }
                },
                "required": ["name", "description", "body"],
                "additionalProperties": false
            }
        }),
    ];

    for tool in mcp_tools {
        tools.push(tool.function_definition());
    }

    tools.push(json!({
        "type": "function",
        "name": "mcp_list",
        "description": format!(
            "List configured MCP servers for the current project at {}.",
            paths.mcp_file.display()
        ),
        "parameters": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    }));

    Value::Array(tools)
}

#[cfg(test)]
mod tests {
    use super::{
        chat_empty_turn_error, durable_memory_suggestion, empty_stream_after_tools_error,
        empty_stream_error, extract_chat_content_text, extract_text, grounding_repair_prompt,
        grounding_violations, handle_chat_sse_block, normalize_tool_name, optional_usize,
        parse_tool_arguments, parse_turn, repeated_tool_repair_prompt, skill_generation_suggestion,
        summarize_completed_tool_turn, tool_checkpoint_after_provider_request_error,
        tool_continuation_prompt, tool_definitions, tool_outputs_need_repair,
        upstream_status_error, ChatStreamTurn, CompletedToolCall, ConsoleObserver,
        NoopStreamCheckpointSink, StreamCheckpointRecorder, ToolCall, ToolDispatchResult,
    };
    use crate::config::AppPaths;
    use crate::id::next_id;
    use crate::session::SessionStore;
    use reqwest::StatusCode;
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn parses_function_call_and_text() {
        let value = json!({
            "id": "resp_1",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "shell",
                    "arguments": "{\"command\":[\"pwd\"]}"
                },
                {
                    "type": "message",
                    "content": [
                        { "type": "output_text", "text": "done" }
                    ]
                }
            ]
        });
        let turn = parse_turn(value).unwrap();
        assert_eq!(turn.response_id.as_deref(), Some("resp_1"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.text.as_deref(), Some("done"));
    }

    #[test]
    fn extracts_text_from_output_text() {
        let value = json!({
            "output_text": "hello"
        });
        assert_eq!(extract_text(&value).as_deref(), Some("hello"));
    }

    #[test]
    fn extracts_text_from_chat_content_parts() {
        let content = json!([
            { "type": "text", "text": "opa" },
            { "type": "output_text", "text": " mundo" }
        ]);
        assert_eq!(
            extract_chat_content_text(Some(&content)).as_deref(),
            Some("opa mundo")
        );
    }

    #[test]
    fn formats_upstream_429_json_for_tui_detection() {
        let error = upstream_status_error(
            "chat completions endpoint",
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"Rate limit exceeded"}}"#,
        );

        assert!(error.contains("returned 429"));
        assert!(error.contains("Rate limit exceeded"));
    }

    #[test]
    fn formats_empty_stream_as_error_not_assistant_text() {
        assert_eq!(
            empty_stream_error("chat completions endpoint"),
            "chat completions endpoint returned an empty response without text or tool calls"
        );
        assert!(
            empty_stream_after_tools_error("responses endpoint", "ls ok")
                .contains("without a final text response")
        );
    }

    #[test]
    fn chat_stream_choice_error_is_reported() {
        let payload = json!({
            "choices": [
                {
                    "error": {
                        "code": 402,
                        "message": "Insufficient credits"
                    }
                }
            ]
        })
        .to_string();
        let block = format!("data: {payload}\n\n");
        let mut state = ChatStreamTurn::default();
        let mut pending = HashMap::new();
        let mut observer = ConsoleObserver;
        let mut stream_recorder = NoopStreamCheckpointSink;

        let err = handle_chat_sse_block(
            &block,
            &mut state,
            &mut pending,
            &mut observer,
            &mut stream_recorder,
        )
        .expect_err("choice errors must not be treated as empty output");

        assert!(err.contains("choice error"));
        assert!(err.contains("Insufficient credits"));
    }

    #[test]
    fn chat_empty_final_turn_keeps_finish_reason() {
        let payload = json!({
            "choices": [
                {
                    "finish_reason": "length",
                    "native_finish_reason": "max_tokens",
                    "delta": {}
                }
            ]
        })
        .to_string();
        let block = format!("data: {payload}\n\n");
        let mut state = ChatStreamTurn::default();
        let mut pending = HashMap::new();
        let mut observer = ConsoleObserver;
        let mut stream_recorder = NoopStreamCheckpointSink;

        handle_chat_sse_block(
            &block,
            &mut state,
            &mut pending,
            &mut observer,
            &mut stream_recorder,
        )
        .unwrap();
        let err = chat_empty_turn_error("chat completions endpoint", &state);

        assert!(err.contains("finish_reason=length"));
        assert!(err.contains("native_finish_reason=max_tokens"));
    }

    #[test]
    fn repeated_recoverable_tool_outputs_request_repair_not_stop() {
        let outputs = vec![
            "Tool error in `shell`\nCommand approval required\nCorrect the tool arguments or choose another tool, then continue.".to_string(),
        ];
        assert!(tool_outputs_need_repair(&outputs));
        assert!(
            repeated_tool_repair_prompt("Last completed tools checkpoint")
                .contains("Continue the same task without stopping")
        );
    }

    #[test]
    fn tool_schema_exposes_subagent() {
        let paths = test_paths();
        let tools = tool_definitions(&paths, &[]);
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()))
            .collect::<Vec<_>>();
        assert!(names.contains(&"subagent"));
        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn tool_checkpoint_preserves_shell_command_and_output() {
        let completed = vec![CompletedToolCall {
            call: ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": ["npm", "install", "@xterm/xterm", "zustand"]
                }),
            },
            result: ToolDispatchResult {
                model_output: "Executed\n```bash\nnpm install @xterm/xterm zustand\n```\n\nadded 4 packages\nfound 0 vulnerabilities\n".to_string(),
                ui_output: "Executed\n```bash\nnpm install @xterm/xterm zustand\n```\n\nadded 4 packages\nfound 0 vulnerabilities\n".to_string(),
            },
            summary: "npm install @xterm/xterm zustand".to_string(),
        }];

        let summary = summarize_completed_tool_turn(&completed);

        assert!(summary.contains("npm install @xterm/xterm zustand"));
        assert!(summary.contains("added 4 packages"));
        assert!(!summary.contains("- shell: Executed\n"));
    }

    #[test]
    fn continuation_prompt_keeps_existing_plan_instead_of_restart() {
        let prompt = tool_continuation_prompt(
            "Last completed tools checkpoint:\n- shell: npm install\n  evidence:\n  ```text\n  added 4 packages\n  ```",
        );

        assert!(prompt.contains("Continue the exact same task"));
        assert!(prompt.contains("Do not restart"));
        assert!(prompt.contains("ACP/WDF recovery"));
        assert!(prompt.contains("next necessary tool call"));
    }

    #[test]
    fn provider_transport_failure_after_tools_returns_checkpoint_text() {
        let text = tool_checkpoint_after_provider_request_error(
            "chat completions endpoint",
            "chat completions request failed before stream: error sending request for url (http://127.0.0.1:3000/v1/chat/completions)",
            "Last completed tools checkpoint:\n- shell: npm install\n  evidence:\n  ```text\n  up to date\n  ```",
        );

        assert!(text.contains("Provider transport failed"));
        assert!(text.contains("latest tool checkpoint"));
        assert!(text.contains("npm install"));
        assert!(text.contains("continue this same session"));
    }

    #[test]
    fn stream_checkpoint_recorder_persists_first_delta_and_agent_state() {
        let paths = test_paths();
        let mut store = SessionStore::new(&paths).unwrap();
        let session = store
            .create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                "seed",
            )
            .unwrap();

        {
            let mut recorder = StreamCheckpointRecorder::new(
                &mut store,
                &paths.project_key,
                &session.id,
                "chat_completions",
            );
            recorder.record_text_delta("oi");
        }

        let timeline = store.timeline(&paths.project_key, &session.id).unwrap();
        assert!(timeline.iter().any(|event| {
            event.kind == "checkpoint"
                && event.command.as_deref() == Some("stream_partial")
                && event
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("\"snapshot_index\":1")
        }));

        let memory = store
            .recall_session_memory(&paths.project_key, &session.id, "", 4)
            .unwrap();
        assert!(memory.iter().any(|item| {
            item.kind == "agent_state"
                && item.content.contains("snapshot=1")
                && item.content.contains("Partial assistant text")
        }));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn memory_and_skill_suggestions_are_detected_without_saving() {
        assert!(durable_memory_suggestion("Prefiro respostas curtas nesse repo.").is_some());
        assert!(skill_generation_suggestion(
            "Sempre que eu pedir release, rode esse workflow repetido."
        )
        .is_some());
        assert!(durable_memory_suggestion("Corrija o bug agora.").is_none());
    }

    #[test]
    fn optional_usize_accepts_strings_for_loose_tool_arguments() {
        let arguments = json!({
            "path": "src/main.rs",
            "start_line": "12",
            "count": 40
        });

        assert_eq!(optional_usize(&arguments, "start_line"), Some(12));
        assert_eq!(optional_usize(&arguments, "count"), Some(40));
        assert_eq!(optional_usize(&arguments, "end_line"), None);
    }

    #[test]
    fn parse_tool_arguments_repairs_missing_closing_brace() {
        let arguments = parse_tool_arguments(r#"{"path":"Next.js","start_line":1"#);

        assert_eq!(
            arguments.get("path").and_then(|value| value.as_str()),
            Some("Next.js")
        );
        assert_eq!(
            arguments.get("start_line").and_then(|value| value.as_u64()),
            Some(1)
        );
    }

    #[test]
    fn normalizes_namespaced_builtin_tool_names() {
        assert_eq!(normalize_tool_name("Box.list_dir", &[]), "list_dir");
        assert_eq!(normalize_tool_name("wire.read_file", &[]), "read_file");
        assert_eq!(normalize_tool_name("TOOLS.apply_patch", &[]), "apply_patch");
        assert_eq!(normalize_tool_name("box:grep_lines", &[]), "grep_lines");
    }

    #[test]
    fn grounding_check_rejects_unavailable_tools_and_placeholders() {
        let paths = test_paths();
        fs::create_dir_all(paths.root_dir.join("src")).unwrap();
        fs::write(
            paths.root_dir.join("Cargo.toml"),
            "[package]\nname = \"wirecli\"\n",
        )
        .unwrap();
        fs::write(paths.root_dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let text = "\
validacao de arquitetura baseada em Lattice
- memory para armazenar decisoes
where rifice could be your crate name
would use `add_file`
validar /prisma/schema.prisma";

        let violations = grounding_violations(&paths, text);
        assert!(violations.iter().any(|item| item.contains("add_file")));
        assert!(violations.iter().any(|item| item.contains("standalone")));
        assert!(violations.iter().any(|item| item.contains("rifice")));
        assert!(violations.iter().any(|item| item.contains("prisma")));
        assert!(violations.iter().any(|item| item.contains("Lattice")));

        let repair = grounding_repair_prompt(
            &paths,
            text,
            Some("Last completed tools:\n- read_file: README.md"),
        )
        .unwrap();
        assert!(repair.contains("Grounding check rejected"));
        assert!(repair.contains("write_file"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn grounding_check_allows_real_tools_and_existing_paths() {
        let paths = test_paths();
        fs::create_dir_all(paths.root_dir.join("src")).unwrap();
        fs::write(
            paths.root_dir.join("Cargo.toml"),
            "[package]\nname = \"wirecli\"\n",
        )
        .unwrap();
        fs::write(paths.root_dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let text = "Use `read_file` on `src/main.rs`, then `apply_patch` and `cargo test`.";
        assert!(grounding_violations(&paths, text).is_empty());
        assert!(
            grounding_violations(&paths, "`add_file` does not exist; use `write_file`.").is_empty()
        );

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-grounding-test-{}", next_id()));
        let wire_dir = root_dir.join(".wirecli");
        let config_dir = wire_dir.join("config");
        let data_dir = wire_dir.join("data");
        AppPaths {
            root_dir: root_dir.clone(),
            project_key: root_dir.display().to_string(),
            wire_dir: wire_dir.clone(),
            config_dir: config_dir.clone(),
            config_file: config_dir.join("config.toml"),
            secret_key_file: config_dir.join("secret.key"),
            theme_file: wire_dir.join("theme.yaml"),
            mcp_file: config_dir.join("mcp_servers.json"),
            hooks_file: wire_dir.join("hooks.json"),
            data_dir: data_dir.clone(),
            history_db: data_dir.join("history.sqlite3"),
            anchor_db: data_dir.join("anchor.sqlite3"),
            memory_context_file: data_dir.join("memory_context.json"),
            sandboxes_dir: wire_dir.join("boxes"),
        }
    }
}
