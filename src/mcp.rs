use crate::config::AppPaths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub server_name: String,
    pub server_command: String,
    pub tool_name: String,
    pub function_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

impl McpToolSpec {
    pub fn function_definition(&self) -> Value {
        json!({
            "type": "function",
            "name": self.function_name,
            "description": self
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool from {}", self.server_name)),
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

impl McpRegistry {
    pub fn load(paths: &AppPaths) -> Result<Self, String> {
        let raw = fs::read_to_string(&paths.mcp_file).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;

        let servers = if value.is_array() {
            serde_json::from_value::<Vec<McpServerConfig>>(value).map_err(|e| e.to_string())?
        } else if value.get("servers").is_some() {
            serde_json::from_value::<McpConfigFile>(value)
                .map_err(|e| e.to_string())?
                .servers
        } else {
            Vec::new()
        };

        Ok(Self { servers })
    }

    pub fn servers(&self) -> &[McpServerConfig] {
        &self.servers
    }

    pub fn discover_tools(&self) -> Result<Vec<McpToolSpec>, String> {
        let mut discovered = Vec::new();
        for server in &self.servers {
            let mut client = McpStdioClient::connect(server)?;
            let result = client.list_tools()?;
            for tool in result {
                discovered.push(McpToolSpec {
                    server_name: server.name.clone(),
                    server_command: server.command.clone(),
                    tool_name: tool.name.clone(),
                    function_name: format!(
                        "mcp__{}__{}",
                        sanitize_ident(&server.name),
                        sanitize_ident(&tool.name)
                    ),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                });
            }
        }
        Ok(discovered)
    }

    pub fn call_tool(
        &self,
        spec: &McpToolSpec,
        arguments: &Value,
    ) -> Result<String, String> {
        let server = self
            .servers
            .iter()
            .find(|server| server.name == spec.server_name && server.command == spec.server_command)
            .ok_or_else(|| format!("unknown MCP server: {}", spec.server_name))?;
        let mut client = McpStdioClient::connect(server)?;
        client.call_tool(&spec.tool_name, arguments)
    }
}

#[derive(Debug, Clone)]
struct McpToolInfo {
    name: String,
    description: Option<String>,
    input_schema: Value,
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
        let stdin = child.stdin.take().ok_or_else(|| "failed to open stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "failed to open stdout".to_string())?;
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
                    "name": "riftcli",
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
        self.stdin.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())
    }

    fn read_response(&mut self, expected_id: u64) -> Result<Value, String> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = self.stdout.read_line(&mut line).map_err(|e| e.to_string())?;
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
    use super::sanitize_ident;

    #[test]
    fn sanitizes_identifiers() {
        assert_eq!(sanitize_ident("qwen coder"), "qwen_coder");
    }
}
