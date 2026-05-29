use crate::agent_tools::{BoxTools, BOX_TOOL_NAMES};
use crate::config::{AppConfig, AppPaths};
use crate::context::Loom;
use crate::mcp::{McpRegistry, McpToolSpec};
use crate::memory::{AnchorInput, AnchorStore};
use crate::prompt::base_developer_prompt;
use crate::sandbox::SandboxManager;
use crate::session::{SessionEvent, SessionStore};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::str;

#[derive(Debug, Clone)]
struct ToolCall {
    call_id: String,
    name: String,
    arguments: Value,
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
}

#[allow(dead_code)]
pub enum AgentEvent<'a> {
    TextDelta(&'a str),
    ToolCallStart { name: &'a str },
    ToolCallResult { name: &'a str, output: &'a str },
}

pub trait AgentObserver {
    fn on_event(&mut self, event: AgentEvent<'_>);
}

pub struct ConsoleObserver;

impl AgentObserver for ConsoleObserver {
    fn on_event(&mut self, event: AgentEvent<'_>) {
        if let AgentEvent::TextDelta(delta) = event {
            print!("{delta}");
            let _ = io::stdout().flush();
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
    let client = Client::new();
    let body = json!({
        "model": config.model,
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
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    let value: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("responses upstream returned {}: {}", status, value));
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

    let sandbox = SandboxManager::new(paths)?;
    let box_summary = sandbox.create(&format!("session-{}", session.id))?;
    store.append_command(
        &paths.project_key,
        &session.id,
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
    let toolbox = BoxTools::new(&sandbox, &anchors);
    let mcp_registry = McpRegistry::load(paths)?;
    let mcp_tools = mcp_registry.discover_tools().unwrap_or_default();
    let loom = Loom::new(paths)?;
    let _ = loom
        .maybe_refresh_summary(paths, config, &store, &session.id)
        .await?;
    let bundle = loom.build(paths, config, &store, &session.id, &prompt)?;
    let output = run_agent_loop(
        paths,
        config,
        &bundle.rendered_prompt,
        &toolbox,
        &box_summary.id,
        &mcp_registry,
        &mcp_tools,
        &mut store,
        &paths.project_key,
        &session.id,
        observer,
    )
    .await?;

    store.append_command(
        &paths.project_key,
        &session.id,
        &["responses.agent".to_string(), config.model.clone()],
        "ok",
        Some(0),
        &output,
        "",
    )?;
    store.append_event(
        &paths.project_key,
        &session.id,
        SessionEvent::assistant(output.clone()),
    )?;

    Ok((session.id, output))
}

async fn run_agent_loop(
    paths: &AppPaths,
    config: &AppConfig,
    prompt: &str,
    toolbox: &BoxTools<'_>,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    store: &mut SessionStore,
    project_key: &str,
    session_id: &str,
    observer: &mut dyn AgentObserver,
) -> Result<String, String> {
    let upstream_url = config.base_url.clone();
    let client = Client::new();
    let instructions = base_developer_prompt();
    let tools = tool_definitions(paths, mcp_tools);
    let mut previous_response_id: Option<String> = None;
    let mut input = json!(prompt);
    let mut last_text = String::new();

    for _ in 0..8 {
        store.append_command(
            project_key,
            session_id,
            &["responses.create".to_string(), config.model.clone()],
            "ok",
            Some(0),
            &format!("instructions=developer tools={}", BOX_TOOL_NAMES.join(",")),
            "",
        )?;
        let body = json!({
            "model": config.model,
            "instructions": instructions,
            "input": input,
            "tools": tools,
            "parallel_tool_calls": true,
            "stream": true,
            "store": false,
            "previous_response_id": previous_response_id,
        });

        let response = client
            .post(format!("{}/responses", upstream_url.trim_end_matches('/')))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("responses upstream returned {}: {}", status, text));
        }

        let turn = consume_stream(response).await?;
        previous_response_id = turn.response_id;

        if !turn.tool_calls.is_empty() {
            let mut outputs = Vec::new();
            for call in turn.tool_calls {
                observer.on_event(AgentEvent::ToolCallStart { name: &call.name });
                let output = dispatch_tool(
                    toolbox,
                    project_key,
                    session_id,
                    box_id,
                    mcp_registry,
                    mcp_tools,
                    call.clone(),
                )?;
                observer.on_event(AgentEvent::ToolCallResult {
                    name: &call.name,
                    output: &output,
                });
                store.append_command(
                    project_key,
                    session_id,
                    &["tool.call".to_string(), call.name.clone()],
                    "ok",
                    Some(0),
                    &output,
                    "",
                )?;
                outputs.push(json!({
                    "type": "function_call_output",
                    "call_id": call.call_id,
                    "output": output,
                }));
            }
            input = Value::Array(outputs);
            continue;
        }

        if !turn.text.trim().is_empty() {
            observer.on_event(AgentEvent::TextDelta(&turn.text));
            if !turn.text.ends_with('\n') {
                observer.on_event(AgentEvent::TextDelta("\n"));
            }
            last_text = turn.text;
            break;
        }

        return Err("responses payload did not contain text or tool calls".to_string());
    }

    if last_text.is_empty() {
        return Err("agent completed without a final text response".to_string());
    }

    Ok(last_text)
}

async fn consume_stream(response: reqwest::Response) -> Result<StreamTurn, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut state = StreamTurn::default();
    let mut pending_calls: HashMap<String, PendingCall> = HashMap::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let chunk = str::from_utf8(&chunk).map_err(|e| e.to_string())?;
        buffer.push_str(chunk);

        while let Some(pos) = buffer.find("\n\n") {
            let event_block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_block.trim().is_empty() {
                continue;
            }

            let payload = extract_sse_data(&event_block);
            let Some(payload) = payload else {
                continue;
            };
            if payload == "[DONE]" {
                continue;
            }

            let event: Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            handle_stream_event(&event, &mut state, &mut pending_calls)?;
        }
    }

    if !buffer.trim().is_empty() {
        if let Some(payload) = extract_sse_data(&buffer) {
            if payload != "[DONE]" {
                let event: Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
                handle_stream_event(&event, &mut state, &mut pending_calls)?;
            }
        }
    }

    Ok(state)
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
                // streamed output is forwarded through the observer in the caller
            }
        }
        "response.output_item.added" => {
            if let Some(item) = event.get("item") {
                if let Some(pending) = parse_pending_call(item) {
                    pending_calls.insert(pending_key(item), pending);
                } else if let Some(text) = extract_message_text(item) {
                    if !text.is_empty() {
                        state.text.push_str(&text);
                        print!("{text}");
                        io::stdout().flush().map_err(|e| e.to_string())?;
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
                pending_calls
                    .entry(item_id.to_string())
                    .or_insert_with(|| PendingCall {
                        call_id: item_id.to_string(),
                        name: String::new(),
                        arguments: String::new(),
                    })
                    .arguments
                    .push_str(delta);
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
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                if let Some(pending) = parse_pending_call(item) {
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
                if state.text.is_empty() {
                    if let Some(text) = extract_text(response) {
                        state.text = text;
                    }
                }
            }

            for pending in pending_calls.values() {
                if pending.name.is_empty() {
                    continue;
                }
                let arguments = if pending.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&pending.arguments)
                        .unwrap_or_else(|_| json!({ "raw": pending.arguments.clone() }))
                };
                state.tool_calls.push(ToolCall {
                    call_id: pending.call_id.clone(),
                    name: pending.name.clone(),
                    arguments,
                });
            }
        }
        "response.failed" | "response.error" | "response.incomplete" => {
            return Err(format!("responses stream reported {event_type}: {event}"));
        }
        _ => {}
    }

    Ok(())
}

fn extract_sse_data(event_block: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in event_block.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start().to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
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
        Some(Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or_else(|_| json!({ "raw": raw }))
        }
        Some(value) => value.clone(),
        None => json!({}),
    };

    Ok(Some(ToolCall {
        call_id,
        name,
        arguments,
    }))
}

fn dispatch_tool(
    toolbox: &BoxTools<'_>,
    project_key: &str,
    session_id: &str,
    box_id: &str,
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    call: ToolCall,
) -> Result<String, String> {
    match call.name.as_str() {
        name if name.starts_with("mcp__") => dispatch_mcp_tool(mcp_registry, mcp_tools, call),
        "mcp_list" => {
            let mut text = String::from("Configured MCP servers\n");
            for server in mcp_registry.servers() {
                text.push_str("- ");
                text.push_str(&server.name);
                text.push_str(" :: ");
                text.push_str(&server.command);
                text.push('\n');
            }
            if mcp_registry.servers().is_empty() {
                text.push_str("(none)\n");
            }
            Ok(text)
        }
        "shell" => {
            let command = extract_string_array(&call.arguments, "command")?;
            toolbox.shell(box_id, &command).map(|response| {
                let mut text = String::from("Executed\n```bash\n");
                text.push_str(&command.join(" "));
                text.push_str("\n```\n\n");
                text.push_str(&response.text);
                text
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
                text
            })
        }
        "list_dir" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.list_dir(box_id, &path).map(|response| {
                let mut text = String::from("Listed\n");
                text.push_str(&path);
                text.push_str("\n\n");
                text.push_str("```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                text
            })
        }
        "read_file" => {
            let path = extract_string(&call.arguments, "path")?;
            toolbox.read_file(box_id, &path).map(|_response| {
                let mut text = String::from("Readed ");
                text.push_str(&path);
                text
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
                text
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
                text
            })
        }
        "remember" => {
            let content = extract_string(&call.arguments, "content")?;
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
            toolbox
                .remember(
                    project_key,
                    AnchorInput {
                        kind,
                        content,
                        tags,
                        importance,
                        confidence,
                        source_session_id: Some(session_id.to_string()),
                    },
                )
                .map(|response| format!("Remembered\n\n{}", response.text))
        }
        "recall" => {
            let query = extract_string(&call.arguments, "query")?;
            toolbox.recall(project_key, &query).map(|response| {
                let mut text = String::from("Recalled\n");
                text.push_str(&query);
                text.push_str("\n\n```text\n");
                text.push_str(&response.text);
                text.push_str("\n```");
                text
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn dispatch_mcp_tool(
    mcp_registry: &McpRegistry,
    mcp_tools: &[McpToolSpec],
    call: ToolCall,
) -> Result<String, String> {
    let spec = mcp_tools
        .iter()
        .find(|tool| tool.function_name == call.name)
        .ok_or_else(|| format!("unknown MCP tool: {}", call.name))?;
    mcp_registry.call_tool(spec, &call.arguments).map(|output| {
        let mut text = String::from("MCP Tool\n");
        text.push_str(&spec.server_name);
        text.push_str("::");
        text.push_str(&spec.tool_name);
        if !output.trim().is_empty() {
            text.push_str("\n\n```text\n");
            text.push_str(&output);
            text.push_str("\n```");
        }
        text
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

fn extract_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field: {key}"))
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
        return Ok(command.split_whitespace().map(|s| s.to_string()).collect());
    }

    Err(format!("missing array field: {key}"))
}

fn tool_definitions(paths: &AppPaths, mcp_tools: &[McpToolSpec]) -> Value {
    let mut tools = vec![
        json!({
            "type": "function",
            "name": "shell",
            "description": "Run a command inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command and arguments to execute."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply a structured patch to files inside the Box.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch_text": {
                        "type": "string",
                        "description": "Patch text in the Rift apply_patch format."
                    }
                },
                "required": ["patch_text"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "list_dir",
            "description": "List files and folders inside the Box workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to inspect."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "read_file",
            "description": "Read a file inside the Box workspace.",
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
            "description": "Write a file inside the Box workspace.",
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
            "name": "remember",
            "description": "Store durable Anchor memory for later sessions.",
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
        })
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
    use super::{extract_text, parse_turn};
    use serde_json::json;

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
}
