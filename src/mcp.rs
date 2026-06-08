use crate::config::AppPaths;
use crate::safekey::write_private_file;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(
        rename = "type",
        alias = "transport",
        default = "default_mcp_transport"
    )]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub http_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub startup_ts: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub server_name: String,
    pub server_transport: String,
    pub server_command: String,
    pub server_url: Option<String>,
    pub tool_name: String,
    pub function_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

impl McpToolSpec {
    pub fn function_definition(&self) -> Value {
        let fallback = format!("MCP tool from {}", self.server_name);
        let base_description = self.description.clone().unwrap_or(fallback);
        let endpoint = self
            .server_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&self.server_command);
        json!({
            "type": "function",
            "name": self.function_name,
            "description": format!(
                "{}\nMCP server: {} via {} ({})\nUse this namespaced MCP tool directly when it matches the task better than shell/file approximations.",
                base_description,
                self.server_name,
                self.server_transport,
                endpoint,
            ),
            "parameters": self.input_schema.clone()
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct McpConfigFile {
    #[serde(default)]
    servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone)]
pub struct McpRegistry {
    servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct McpDiscoveryReport {
    pub tools: Vec<McpToolSpec>,
    pub errors: Vec<String>,
}

impl McpRegistry {
    pub fn load(paths: &AppPaths) -> Result<Self, String> {
        let mut servers = if paths.mcp_file.exists() {
            let raw = fs::read_to_string(&paths.mcp_file).map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if value.is_array() {
                serde_json::from_value::<Vec<McpServerConfig>>(value).map_err(|e| e.to_string())?
            } else if value.get("servers").is_some() {
                serde_json::from_value::<McpConfigFile>(value)
                    .map_err(|e| e.to_string())?
                    .servers
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        for server in load_toml_mcp_servers(paths)? {
            if let Some(existing) = servers.iter_mut().find(|item| item.name == server.name) {
                *existing = server;
            } else {
                servers.push(server);
            }
        }

        Ok(Self { servers })
    }

    pub fn servers(&self) -> &[McpServerConfig] {
        &self.servers
    }

    pub fn save(paths: &AppPaths, servers: &[McpServerConfig]) -> Result<(), String> {
        save_toml_mcp_servers(paths, servers)
    }

    pub fn add_server(paths: &AppPaths, server: McpServerConfig) -> Result<(), String> {
        let registry = Self::load(paths)?;
        let mut servers = registry.servers.clone();
        if let Some(existing) = servers.iter_mut().find(|item| item.name == server.name) {
            *existing = server;
        } else {
            servers.push(server);
        }
        Self::save(paths, &servers)
    }

    pub fn discover_tools(&self) -> Result<Vec<McpToolSpec>, String> {
        let report = self.discover_tools_report();
        if report.tools.is_empty() && !report.errors.is_empty() {
            return Err(report.errors.join("\n"));
        }
        Ok(report.tools)
    }

    pub fn discover_tools_report(&self) -> McpDiscoveryReport {
        if self.servers.is_empty() {
            return McpDiscoveryReport::default();
        }

        let (tx, rx) = mpsc::channel();
        for server in self.servers.clone() {
            let tx = tx.clone();
            thread::spawn(move || {
                let name = server.name.clone();
                let result = discover_server_tools(&server);
                let _ = tx.send((name, result));
            });
        }
        drop(tx);

        let timeout = self
            .servers
            .iter()
            .filter_map(|server| server.startup_ts)
            .max()
            .map(Duration::from_secs)
            .unwrap_or_else(mcp_discovery_timeout);
        let deadline = Instant::now() + timeout;
        let mut report = McpDiscoveryReport::default();

        for _ in 0..self.servers.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                report.errors.push(format!(
                    "MCP discovery timed out after {}ms",
                    timeout.as_millis()
                ));
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok((_name, Ok(mut tools))) => report.tools.append(&mut tools),
                Ok((name, Err(err))) => report.errors.push(format!("{name}: {err}")),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report.errors.push(format!(
                        "MCP discovery timed out after {}ms",
                        timeout.as_millis()
                    ));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        report
    }

    pub fn call_tool(&self, spec: &McpToolSpec, arguments: &Value) -> Result<String, String> {
        let server = self
            .servers
            .iter()
            .find(|server| {
                server.name == spec.server_name
                    && server.transport == spec.server_transport
                    && server.command == spec.server_command
                    && server.url == spec.server_url
            })
            .ok_or_else(|| format!("unknown MCP server: {}", spec.server_name))?;
        let mut client = McpClient::connect(server)?;
        client.call_tool(&spec.tool_name, arguments)
    }
}

fn discover_server_tools(server: &McpServerConfig) -> Result<Vec<McpToolSpec>, String> {
    let mut client = McpClient::connect(server)?;
    let result = client.list_tools()?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    Ok(result
        .into_iter()
        .map(|tool| {
            let base_function_name = format!(
                "mcp__{}__{}",
                sanitize_ident(&server.name),
                sanitize_ident(&tool.name)
            );
            let count = counts.entry(base_function_name.clone()).or_insert(0);
            *count += 1;
            let function_name = if *count == 1 {
                base_function_name
            } else {
                format!("{base_function_name}_{}", *count)
            };
            McpToolSpec {
                server_name: server.name.clone(),
                server_transport: server.transport.clone(),
                server_command: server.command.clone(),
                server_url: server.url.clone(),
                tool_name: tool.name.clone(),
                function_name,
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            }
        })
        .collect())
}

fn mcp_discovery_timeout() -> Duration {
    std::env::var("WIRE_MCP_DISCOVERY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(2500))
}

#[derive(Debug, Clone)]
struct McpToolInfo {
    name: String,
    description: Option<String>,
    input_schema: Value,
}

enum McpClient {
    Stdio(McpStdioClient),
    Http(McpHttpClient),
}

impl McpClient {
    fn connect(server: &McpServerConfig) -> Result<Self, String> {
        match server.transport.as_str() {
            "http" | "https" => Ok(Self::Http(McpHttpClient::connect(server)?)),
            _ => Ok(Self::Stdio(McpStdioClient::connect(server)?)),
        }
    }

    fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, String> {
        match self {
            Self::Stdio(client) => client.list_tools(),
            Self::Http(client) => client.list_tools(),
        }
    }

    fn call_tool(&mut self, tool_name: &str, arguments: &Value) -> Result<String, String> {
        match self {
            Self::Stdio(client) => client.call_tool(tool_name, arguments),
            Self::Http(client) => client.call_tool(tool_name, arguments),
        }
    }
}

struct McpStdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpStdioClient {
    fn connect(server: &McpServerConfig) -> Result<Self, String> {
        let mut command = Command::new(&server.command);
        command.args(&server.args);
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &server.env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open stdout".to_string())?;
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "wirecli",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }))?;
        let _response = self.read_response(id)?;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, String> {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        }))?;
        let response = self.read_response(id)?;
        let tools_value = response
            .get("result")
            .and_then(|result| result.get("tools"))
            .or_else(|| response.get("tools"))
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));

        let mut tools = Vec::new();
        if let Some(array) = tools_value.as_array() {
            for tool in array {
                let name = tool
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                tools.push(McpToolInfo {
                    name,
                    description: tool
                        .get("description")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    input_schema: tool
                        .get("inputSchema")
                        .or_else(|| tool.get("input_schema"))
                        .cloned()
                        .unwrap_or_else(|| {
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": true
                            })
                        }),
                });
            }
        }
        Ok(tools)
    }

    fn call_tool(&mut self, tool_name: &str, arguments: &Value) -> Result<String, String> {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        }))?;
        let response = self.read_response(id)?;
        Ok(flatten_tool_result(&response))
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())
    }

    fn read_response(&mut self, expected_id: u64) -> Result<Value, String> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if bytes == 0 {
                return Err("MCP server closed the stream".to_string());
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
            if value.get("id").and_then(|id| id.as_u64()) == Some(expected_id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("MCP error: {error}"));
                }
                return Ok(value);
            }
        }
    }
}

struct McpHttpClient {
    client: reqwest::blocking::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
}

impl McpHttpClient {
    fn connect(server: &McpServerConfig) -> Result<Self, String> {
        let url = server
            .url
            .clone()
            .ok_or_else(|| format!("MCP server {} missing url", server.name))?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|e| e.to_string())?;
        let mut connected = Self {
            client,
            url,
            headers: server.http_headers.clone(),
            session_id: None,
            next_id: 1,
        };
        connected.initialize()?;
        Ok(connected)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_id();
        let _ = self.request(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "wirecli",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            Some(id),
        )?;
        self.send_notification(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, String> {
        let id = self.next_id();
        let response = self.request(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/list",
                "params": {}
            }),
            Some(id),
        )?;
        let tools_value = response
            .get("result")
            .and_then(|result| result.get("tools"))
            .or_else(|| response.get("tools"))
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));

        let mut tools = Vec::new();
        if let Some(array) = tools_value.as_array() {
            for tool in array {
                let name = tool
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                tools.push(McpToolInfo {
                    name,
                    description: tool
                        .get("description")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    input_schema: tool
                        .get("inputSchema")
                        .or_else(|| tool.get("input_schema"))
                        .cloned()
                        .unwrap_or_else(|| {
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": true
                            })
                        }),
                });
            }
        }
        Ok(tools)
    }

    fn call_tool(&mut self, tool_name: &str, arguments: &Value) -> Result<String, String> {
        let id = self.next_id();
        let response = self.request(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": arguments
                }
            }),
            Some(id),
        )?;
        Ok(flatten_tool_result(&response))
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request(&mut self, body: &Value, expected_id: Option<u64>) -> Result<Value, String> {
        let mut request = self.client.post(&self.url).json(body);
        request = request.header(ACCEPT, "application/json, text/event-stream");
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session_id) = self.session_id.as_ref() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request.send().map_err(|e| e.to_string())?;
        let status = response.status();
        if let Some(session_id) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_string());
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let text = response.text().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("MCP HTTP error {}: {}", status, text));
        }
        if content_type.contains("text/event-stream") || text.contains("data:") {
            return parse_mcp_event_stream(&text, expected_id);
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if let Some(error) = value.get("error") {
            return Err(format!("MCP error: {error}"));
        }
        if let Some(id) = expected_id {
            if value.get("id").and_then(|value| value.as_u64()) != Some(id) {
                return Err(format!("MCP HTTP response did not match request id {}", id));
            }
        }
        Ok(value)
    }

    fn send_notification(&self, body: &Value) -> Result<(), String> {
        let mut request = self.client.post(&self.url).json(body);
        request = request.header(ACCEPT, "application/json, text/event-stream");
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session_id) = self.session_id.as_ref() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request.send().map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            let text = response.text().unwrap_or_default();
            return Err(format!("MCP HTTP notification failed: {}", text));
        }
        Ok(())
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn flatten_tool_result(response: &Value) -> String {
    let result = response.get("result").unwrap_or(response);
    if let Some(content) = result.get("content").and_then(|value| value.as_array()) {
        let mut lines = Vec::new();
        for item in content {
            if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                lines.push(text.to_string());
            } else {
                lines.push(serde_json::to_string_pretty(item).unwrap_or_default());
            }
        }
        if !lines.is_empty() {
            return lines.join("\n");
        }
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

fn parse_mcp_event_stream(text: &str, expected_id: Option<u64>) -> Result<Value, String> {
    let mut last_value: Option<Value> = None;
    for block in text.split("\n\n") {
        let payload = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        if let Some(id) = expected_id {
            if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("MCP error: {error}"));
                }
                return Ok(value);
            }
        }
        last_value = Some(value);
    }

    last_value.ok_or_else(|| "MCP event stream returned no payload".to_string())
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

fn load_toml_mcp_servers(paths: &AppPaths) -> Result<Vec<McpServerConfig>, String> {
    let path = paths.config_file.clone();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            current = section
                .strip_prefix("mcp_servers.")
                .map(|name| sanitize_server_name(name));
            continue;
        }
        let Some(name) = current.clone() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let server = servers
            .entry(name.clone())
            .or_insert_with(|| McpServerConfig {
                name: name.clone(),
                transport: "stdio".to_string(),
                command: String::new(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                url: None,
                http_headers: BTreeMap::new(),
                startup_ts: None,
            });
        match key {
            "url" => {
                server.url = Some(strip_toml_string(value));
                server.transport = "http".to_string();
            }
            "command" => {
                server.command = strip_toml_string(value);
                server.transport = "stdio".to_string();
            }
            "args" => server.args = parse_toml_string_array(value),
            "startup_ts" | "startup_timeout" => {
                server.startup_ts = strip_toml_string(value).parse::<u64>().ok()
            }
            "cwd" => server.cwd = Some(PathBuf::from(strip_toml_string(value))),
            _ => {}
        }
    }

    Ok(servers.into_values().collect())
}

fn save_toml_mcp_servers(paths: &AppPaths, servers: &[McpServerConfig]) -> Result<(), String> {
    let existing = fs::read_to_string(&paths.config_file).unwrap_or_default();
    let mut out = strip_mcp_sections(&existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !servers.is_empty() {
        out.push_str("\n# MCP servers\n");
    }
    for server in servers {
        out.push_str("\n[mcp_servers.");
        out.push_str(&sanitize_server_name(&server.name));
        out.push_str("]\n");
        if server.transport == "http" || server.url.as_deref().unwrap_or("").starts_with("http") {
            if let Some(url) = server
                .url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                push_toml_string(&mut out, "url", url);
            }
        } else {
            push_toml_string(&mut out, "command", &server.command);
            if !server.args.is_empty() {
                push_toml_array(&mut out, "args", &server.args);
            }
            if let Some(cwd) = server.cwd.as_ref() {
                push_toml_string(&mut out, "cwd", &cwd.display().to_string());
            }
        }
        if let Some(startup_ts) = server.startup_ts {
            out.push_str("startup_ts = ");
            out.push_str(&startup_ts.to_string());
            out.push('\n');
        }
    }
    write_private_file(&paths.config_file, out.as_bytes())
}

fn strip_mcp_sections(raw: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            skip = section.starts_with("mcp_servers.") || section.starts_with("mcp_server.");
        }
        if !skip {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn push_toml_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = \"");
    out.push_str(&toml_escape(value));
    out.push_str("\"\n");
}

fn push_toml_array(out: &mut String, key: &str, values: &[String]) {
    out.push_str(key);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(&toml_escape(value));
        out.push('"');
    }
    out.push_str("]\n");
}

fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn strip_toml_string(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

fn parse_toml_string_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(strip_toml_string)
        .filter(|value| !value.is_empty())
        .collect()
}

fn sanitize_server_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn sanitize_ident(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::{sanitize_ident, McpRegistry, McpServerConfig};
    use crate::config::AppPaths;
    use crate::id::next_id;
    use std::collections::BTreeMap;
    use std::fs;

    #[test]
    fn sanitizes_identifiers() {
        assert_eq!(sanitize_ident("qwen coder"), "qwen_coder");
    }

    #[test]
    fn saves_mcp_servers_into_config_toml() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            &paths.config_file,
            "model_provider = \"openrouter\"\n\n[features]\nmemories = true\n",
        )
        .unwrap();
        McpRegistry::save(
            &paths,
            &[McpServerConfig {
                name: "local docs".to_string(),
                transport: "stdio".to_string(),
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@example/local-docs".to_string()],
                cwd: None,
                env: BTreeMap::new(),
                url: None,
                http_headers: BTreeMap::new(),
                startup_ts: Some(120),
            }],
        )
        .unwrap();

        let raw = fs::read_to_string(&paths.config_file).unwrap();
        assert!(raw.contains("model_provider = \"openrouter\""));
        assert!(raw.contains("[mcp_servers.local-docs]"));
        assert!(raw.contains("command = \"npx\""));
        assert!(!paths.mcp_file.exists());

        let registry = McpRegistry::load(&paths).unwrap();
        assert_eq!(registry.servers().len(), 1);
        assert_eq!(registry.servers()[0].name, "local-docs");

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-mcp-test-{}", next_id()));
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
