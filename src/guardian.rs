use crate::config::AppConfig;
use crate::providers::provider_headers;
use reqwest::blocking::Client;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use serde_json::{json, Value};
use std::env;
use std::path::Path;
use std::time::Duration;

const DEFAULT_GUARDIAN_MODEL: &str = "openrouter/free";

#[derive(Debug, Clone)]
pub struct GuardianDecision {
    pub allow: bool,
    pub risk: String,
    pub reason: String,
}

pub fn review_command(
    config: &AppConfig,
    command: &[String],
    workspace: &Path,
    reason: &str,
    context: &str,
) -> Result<GuardianDecision, String> {
    let model = env::var("WIRE_GUARDIAN_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GUARDIAN_MODEL.to_string());
    let url = env::var("WIRE_GUARDIAN_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}/chat/completions",
                config.base_url.trim().trim_end_matches('/')
            )
        });
    if config.base_url.trim().is_empty() && env::var("WIRE_GUARDIAN_URL").is_err() {
        return Err("Guardian mode requires a configured provider; run `wirecli login` or set an API key provider".to_string());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut headers = provider_headers(config)?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(title) = HeaderValue::from_str("Wire CLI Guardian") {
        headers.insert("X-OpenRouter-Title", title);
    }

    let body = json!({
        "model": model,
        "stream": false,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "wire_guardian_command_decision",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "boolean" },
                        "risk": { "type": "string", "enum": ["low", "medium", "high", "critical"] },
                        "reason": { "type": "string" }
                    },
                    "required": ["allow", "risk", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "messages": [
            {
                "role": "system",
                "content": "You are Wire CLI Guardian. Decide whether a local coding agent may execute a command. Return only JSON matching the schema. Deny commands that escape the project, use privilege escalation, alter host services, exfiltrate data, download external code without explicit user-approved installation intent, run destructive filesystem operations, or hide intent behind shells/metacharacters."
            },
            {
                "role": "user",
                "content": format!(
                    "Command argv: {}\nWorkspace: {}\nAgent reason: {}\nContext: {}\nDecision policy: allow only if the command is necessary, project-local, non-destructive, and consistent with a normal development workflow.",
                    serde_json::to_string(command).unwrap_or_else(|_| "[]".to_string()),
                    workspace.display(),
                    reason.trim(),
                    context.trim()
                )
            }
        ]
    });

    let response = client
        .post(url)
        .headers(headers)
        .json(&body)
        .send()
        .map_err(|e| format!("Guardian request failed: {e}"))?;
    let status = response.status();
    let value: Value = response.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Guardian returned {status}: {value}"));
    }
    let content = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .ok_or_else(|| "Guardian response missing message content".to_string())?;
    parse_guardian_decision(content)
}

fn parse_guardian_decision(content: &str) -> Result<GuardianDecision, String> {
    let value: Value = serde_json::from_str(content.trim()).map_err(|e| {
        format!(
            "Guardian returned non-JSON decision: {e}; content={}",
            content.trim()
        )
    })?;
    let allow = value
        .get("allow")
        .and_then(|value| value.as_bool())
        .ok_or_else(|| "Guardian decision missing boolean allow".to_string())?;
    let risk = value
        .get("risk")
        .and_then(|value| value.as_str())
        .unwrap_or("high")
        .to_string();
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("Guardian did not provide a reason")
        .to_string();
    Ok(GuardianDecision {
        allow,
        risk,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_guardian_decision;

    #[test]
    fn parses_guardian_json_decision() {
        let decision =
            parse_guardian_decision(r#"{"allow":true,"risk":"low","reason":"project local"}"#)
                .unwrap();
        assert!(decision.allow);
        assert_eq!(decision.risk, "low");
    }
}
