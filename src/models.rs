use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub owned_by: Option<String>,
    pub context_window: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub prompt_price_per_million: Option<f64>,
    pub completion_price_per_million: Option<f64>,
    pub capabilities: Vec<String>,
}

impl ModelInfo {
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.clone())
    }

    pub fn subtitle(&self) -> String {
        let mut parts = Vec::new();
        parts.push(self.id.clone());
        if let Some(owned_by) = &self.owned_by {
            if !owned_by.is_empty() {
                parts.push(format!("owner {owned_by}"));
            }
        }
        if let Some(context_window) = self.context_window {
            parts.push(format!("ctx {}", compact_number(context_window)));
        }
        if let Some(max_completion_tokens) = self.max_completion_tokens {
            parts.push(format!("out {}", compact_number(max_completion_tokens)));
        }
        if let Some(price) = self.price_label() {
            parts.push(price);
        }
        if !self.capabilities.is_empty() {
            parts.push(self.capabilities.join(" "));
        }
        parts.join("  ·  ")
    }

    pub fn price_label(&self) -> Option<String> {
        match (
            self.prompt_price_per_million,
            self.completion_price_per_million,
        ) {
            (Some(prompt), Some(completion)) => Some(format!(
                "{} input / {} output",
                compact_usd_per_million(prompt),
                compact_usd_per_million(completion)
            )),
            (Some(prompt), None) => Some(format!("{} input", compact_usd_per_million(prompt))),
            (None, Some(completion)) => {
                Some(format!("{} output", compact_usd_per_million(completion)))
            }
            (None, None) => None,
        }
    }

    pub fn is_large_or_premium(&self) -> bool {
        let id = self.id.to_ascii_lowercase();
        let name = self
            .name
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        id.contains("opus")
            || name.contains("opus")
            || self.context_window.unwrap_or(0) >= 500_000
            || self.prompt_price_per_million.unwrap_or(0.0) >= 4.0
            || self.completion_price_per_million.unwrap_or(0.0) >= 20.0
    }
}

pub fn parse_models(value: &Value, fallback_id: &str) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    let has_openai_data = value.get("data").and_then(|v| v.as_array()).is_some();
    if let Some(data) = value.get("data").and_then(|v| v.as_array()) {
        for model in data {
            if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                models.push(ModelInfo {
                    id: id.to_string(),
                    name: model
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string()),
                    owned_by: model
                        .get("owned_by")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string()),
                    context_window: model
                        .get("context_window")
                        .or_else(|| model.get("context_length"))
                        .or_else(|| model.pointer("/top_provider/context_length"))
                        .or_else(|| model.pointer("/endpoints/0/context_length"))
                        .and_then(|v| v.as_u64()),
                    max_completion_tokens: max_completion_tokens(model),
                    prompt_price_per_million: price_per_million(model, &["prompt", "input"]),
                    completion_price_per_million: price_per_million(
                        model,
                        &["completion", "output"],
                    ),
                    capabilities: extract_capabilities(model),
                });
            }
        }
    }
    if models.is_empty() && !has_openai_data {
        models.push(ModelInfo {
            id: fallback_id.to_string(),
            name: Some(fallback_id.to_string()),
            owned_by: None,
            context_window: None,
            max_completion_tokens: None,
            prompt_price_per_million: None,
            completion_price_per_million: None,
            capabilities: Vec::new(),
        });
    }
    models
}

pub fn endpoint_error_message(
    endpoint: &str,
    status: u16,
    value: &Value,
    login_message: &str,
) -> String {
    let message = error_message(value);

    if response_requires_login(status, value) {
        return login_message.to_string();
    }

    if !message.is_empty() {
        return format!("{endpoint} returned {status}: {message}");
    }

    format!("{endpoint} returned {status}: {value}")
}

pub fn response_requires_login(status: u16, value: &Value) -> bool {
    let error = value.get("error");
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();

    matches!(
        code,
        "missing_api_key" | "invalid_api_key" | "unauthorized" | "forbidden"
    ) || status == 401
        || status == 403
}

pub fn error_message(value: &Value) -> String {
    value
        .get("error")
        .and_then(|value| value.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

pub fn find_model<'a>(models: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    models.iter().find(|model| model.id == id).or_else(|| {
        models.iter().find(|model| {
            model
                .id
                .rsplit_once(':')
                .map(|(_, suffix)| suffix == id)
                .unwrap_or(false)
        })
    })
}

pub fn estimated_tokens(value: &str) -> u64 {
    if value.trim().is_empty() {
        return 0;
    }
    let chars = value.chars().count() as u64;
    let words = value.split_whitespace().count() as u64;
    let by_chars = chars.saturating_add(3) / 4;
    by_chars.max(words)
}

pub fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        let whole = value / 1_000_000;
        let tenths = (value % 1_000_000) / 100_000;
        if tenths == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{tenths}M")
        }
    } else if value >= 1_000 {
        let whole = value / 1_000;
        let tenths = (value % 1_000) / 100;
        if tenths == 0 {
            format!("{whole}K")
        } else {
            format!("{whole}.{tenths}K")
        }
    } else {
        value.to_string()
    }
}

pub fn compact_usd_per_million(value: f64) -> String {
    let amount = if value >= 10.0 {
        format!("{value:.0}")
    } else if value >= 1.0 {
        format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        format!("{value:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    };
    format!("${amount}/M")
}

fn price_per_million(model: &Value, names: &[&str]) -> Option<f64> {
    for name in names {
        let value = model
            .pointer(&format!("/pricing/{name}"))
            .or_else(|| model.pointer(&format!("/top_provider/pricing/{name}")))
            .or_else(|| model.pointer(&format!("/endpoints/0/pricing/{name}")));
        if let Some(price) = value.and_then(parse_price_value) {
            return Some(price * 1_000_000.0);
        }
    }
    None
}

fn parse_price_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
        .filter(|price| price.is_finite() && *price >= 0.0)
}

fn max_completion_tokens(model: &Value) -> Option<u64> {
    model
        .get("max_completion_tokens")
        .or_else(|| model.get("max_output_tokens"))
        .or_else(|| model.get("max_output_length"))
        .or_else(|| model.pointer("/top_provider/max_completion_tokens"))
        .or_else(|| model.pointer("/top_provider/max_output_tokens"))
        .or_else(|| model.pointer("/top_provider/max_output_length"))
        .or_else(|| model.pointer("/endpoints/0/max_completion_tokens"))
        .or_else(|| model.pointer("/endpoints/0/max_output_tokens"))
        .or_else(|| model.pointer("/endpoints/0/max_output_length"))
        .and_then(|v| v.as_u64())
}

fn extract_capabilities(model: &Value) -> Vec<String> {
    let Some(capabilities) = model.get("capabilities").and_then(|v| v.as_object()) else {
        let mut out = extract_supported_parameters(model.get("supported_parameters"));
        append_supported_features(&mut out, model.get("supported_features"));
        append_supported_features(&mut out, model.pointer("/endpoints/0/supported_parameters"));
        return out;
    };
    let order = [
        ("tools", "tools"),
        ("vision", "vision"),
        ("document", "doc"),
        ("video", "video"),
        ("audio", "audio"),
        ("thinking", "think"),
        ("reasoning", "reason"),
        ("search", "search"),
        ("citations", "cite"),
        ("json", "json"),
        ("structured", "structured"),
        ("tool_choice", "tool_choice"),
    ];
    let mut out = Vec::new();
    for (key, label) in order {
        if capabilities
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push(label.to_string());
        }
    }
    out
}

fn append_supported_features(out: &mut Vec<String>, value: Option<&Value>) {
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return;
    };
    let labels = [
        ("tools", "tools"),
        ("tool_choice", "tool_choice"),
        ("response_format", "json"),
        ("json_mode", "json"),
        ("structured_outputs", "structured"),
        ("reasoning", "reasoning"),
        ("web_search", "search"),
    ];
    for (key, label) in labels {
        if items.iter().any(|value| value.as_str() == Some(key))
            && !out.iter().any(|value| value == label)
        {
            out.push(label.to_string());
        }
    }
}

fn extract_supported_parameters(value: Option<&Value>) -> Vec<String> {
    let Some(parameters) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let order = [
        ("tools", "tools"),
        ("tool_choice", "tool_choice"),
        ("response_format", "json"),
        ("structured_outputs", "structured"),
        ("reasoning", "reasoning"),
    ];
    for (key, label) in order {
        if parameters.iter().any(|value| value.as_str() == Some(key)) {
            out.push(label.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{estimated_tokens, parse_models, response_requires_login};

    #[test]
    fn detects_invalid_api_key_as_login_required() {
        let value = serde_json::json!({
            "error": {
                "code": "invalid_api_key",
                "message": "Invalid API key."
            }
        });

        assert!(response_requires_login(401, &value));
    }

    #[test]
    fn keeps_openai_empty_model_list_empty() {
        let value = serde_json::json!({
            "object": "list",
            "data": []
        });

        assert!(parse_models(&value, "gpt-4.1").is_empty());
    }

    #[test]
    fn parses_openrouter_context_and_output_limits() {
        let value = serde_json::json!({
            "data": [{
                "id": "openrouter/free",
                "name": "Free",
                "context_length": 128000,
                "top_provider": { "max_completion_tokens": 8192 },
                "supported_parameters": ["tools", "response_format"]
            }]
        });

        let models = parse_models(&value, "fallback");
        assert_eq!(models[0].context_window, Some(128000));
        assert_eq!(models[0].max_completion_tokens, Some(8192));
        assert!(models[0].capabilities.contains(&"tools".to_string()));
        assert!(models[0].capabilities.contains(&"json".to_string()));
    }

    #[test]
    fn parses_openrouter_pricing_and_marks_premium_models() {
        let value = serde_json::json!({
            "data": [{
                "id": "anthropic/claude-opus-4.8",
                "name": "Claude Opus 4.8",
                "context_length": 1000000,
                "pricing": {
                    "prompt": "0.000005",
                    "completion": "0.000025"
                }
            }]
        });

        let models = parse_models(&value, "fallback");
        assert_eq!(models[0].prompt_price_per_million, Some(5.0));
        assert_eq!(models[0].completion_price_per_million, Some(25.0));
        assert_eq!(
            models[0].price_label().as_deref(),
            Some("$5/M input / $25/M output")
        );
        assert!(models[0].is_large_or_premium());
    }

    #[test]
    fn estimates_non_empty_text_tokens() {
        assert!(estimated_tokens("a long enough sentence for rough token estimation") > 0);
    }
}
