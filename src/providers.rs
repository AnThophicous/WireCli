use crate::config::{AppConfig, CustomModelProvider};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    Responses,
    ChatCompletions,
    AnthropicMessages,
}

impl ProviderProtocol {
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat_completions" | "chat-completions" | "completions" | "completion" => {
                Self::ChatCompletions
            }
            "chat_completations" | "chat-completations" | "completations" | "completation" => {
                Self::ChatCompletions
            }
            "anthropic_messages" | "anthropic-messages" | "messages" => Self::AnthropicMessages,
            _ => Self::Responses,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    HealthEndpoint,
    ModelsEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseState {
    Stateful,
    Stateless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallingMode {
    Native,
    CLI,
}

#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub api_key_env: Option<String>,
    pub health: ProviderHealth,
    pub response_state: ResponseState,
    pub protocol: ProviderProtocol,
    pub supports_prompt_cache: bool,
    pub supports_parallel_tool_calls: bool,
    pub tool_calling_mode: ToolCallingMode,
    pub notes: Vec<String>,
}

pub fn provider_is_official(provider: &str) -> bool {
    builtin_providers()
        .iter()
        .any(|profile| profile.id == normalize_provider_id(provider))
}

pub fn provider_uses_openrouter_pkce(provider: &str) -> bool {
    normalize_provider_id(provider) == "openrouter"
}

pub fn provider_model_mismatch(config: &AppConfig) -> Option<String> {
    let model = config.model.trim();
    if model.is_empty() {
        return None;
    }

    let provider_id = normalize_provider_id(&config.provider);
    let profile = available_providers(config)
        .into_iter()
        .find(|profile| normalize_provider_id(&profile.id) == provider_id);
    let explicitly_listed = profile
        .as_ref()
        .map(|profile| profile.models.iter().any(|item| item == model))
        .unwrap_or(false);
    let is_custom_provider = config
        .model_providers
        .iter()
        .any(|custom| normalize_provider_id(&custom.id) == provider_id);
    let namespace = model_namespace(model);

    if provider_id != "openrouter"
        && namespace.is_some()
        && !explicitly_listed
        && (!is_custom_provider || namespace == Some("openrouter"))
    {
        return Some(format!(
            "Model `{model}` looks like an OpenRouter catalog id, but provider `{}` is selected. Choose a model from `{}` before sending.",
            config.provider, config.provider
        ));
    }

    if let Some(profile) = profile {
        let custom_has_strict_catalog = is_custom_provider && !profile.models.is_empty();
        if custom_has_strict_catalog && !explicitly_listed {
            return Some(format!(
                "Model `{model}` is not listed for custom provider `{}`. Choose one of the configured provider models before sending.",
                profile.id
            ));
        }
    }

    None
}

pub fn available_providers(config: &AppConfig) -> Vec<ProviderProfile> {
    let mut providers = builtin_providers();
    for custom in &config.model_providers {
        if providers.iter().any(|provider| provider.id == custom.id) {
            continue;
        }
        providers.push(custom_provider_profile(custom));
    }
    providers
}

pub fn builtin_providers() -> Vec<ProviderProfile> {
    vec![
        ProviderProfile {
            id: "openrouter".to_string(),
            label: "OpenRouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            default_model: "openai/gpt-5.2".to_string(),
            models: vec![
                "openai/gpt-5.2".to_string(),
                "anthropic/claude-sonnet-4.5".to_string(),
                "google/gemini-3.5-pro".to_string(),
            ],
            api_key_env: None,
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec![
                "primary provider".to_string(),
                "PKCE login creates a user-controlled OpenRouter key".to_string(),
            ],
        },
        ProviderProfile {
            id: "openai".to_string(),
            label: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            default_model: "gpt-5.2".to_string(),
            models: vec!["gpt-5.2".to_string(), "gpt-5.2-mini".to_string()],
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateful,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: true,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["Responses remains available for advanced OpenAI models".to_string()],
        },
        ProviderProfile {
            id: "deepseek".to_string(),
            label: "DeepSeek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            default_model: "deepseek-v4-pro".to_string(),
            models: vec!["deepseek-v4-pro".to_string(), "deepseek-chat".to_string()],
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["OpenAI-compatible chat completions".to_string()],
        },
        ProviderProfile {
            id: "anthropic".to_string(),
            label: "Claude / Anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            default_model: "claude-sonnet-4-5-20250929".to_string(),
            models: vec![
                "claude-sonnet-4-5-20250929".to_string(),
                "claude-haiku-4-5-20251001".to_string(),
            ],
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::AnthropicMessages,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["native Anthropic Messages API".to_string()],
        },
        ProviderProfile {
            id: "zai".to_string(),
            label: "Z.AI GLM".to_string(),
            base_url: "https://api.z.ai/api/paas/v4".to_string(),
            default_model: "glm-5.1".to_string(),
            models: vec!["glm-5.1".to_string()],
            api_key_env: Some("ZAI_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["OpenAI-compatible GLM-5.1 chat completions".to_string()],
        },
        ProviderProfile {
            id: "google".to_string(),
            label: "Google Gemini".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            default_model: "gemini-3.5-flash".to_string(),
            models: vec!["gemini-3.5-flash".to_string(), "gemini-3.5-pro".to_string()],
            api_key_env: Some("GEMINI_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["Gemini OpenAI-compatible endpoint".to_string()],
        },
        ProviderProfile {
            id: "qwen".to_string(),
            label: "Qwen Cloud".to_string(),
            base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".to_string(),
            default_model: "qwen3.7-plus".to_string(),
            models: vec!["qwen3.7-plus".to_string(), "qwen3.7-coder".to_string()],
            api_key_env: Some("DASHSCOPE_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["Qwen OpenAI-compatible chat completions".to_string()],
        },
        ProviderProfile {
            id: "mistral".to_string(),
            label: "Mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            default_model: "mistral-large-latest".to_string(),
            models: vec![
                "mistral-large-latest".to_string(),
                "codestral-latest".to_string(),
            ],
            api_key_env: Some("MISTRAL_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["extra OpenAI-compatible provider".to_string()],
        },
        ProviderProfile {
            id: "xai".to_string(),
            label: "xAI".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            default_model: "grok-4".to_string(),
            models: vec!["grok-4".to_string(), "grok-4-fast".to_string()],
            api_key_env: Some("XAI_API_KEY".to_string()),
            health: ProviderHealth::ModelsEndpoint,
            response_state: ResponseState::Stateless,
            protocol: ProviderProtocol::ChatCompletions,
            supports_prompt_cache: false,
            supports_parallel_tool_calls: true,
            tool_calling_mode: ToolCallingMode::Native,
            notes: vec!["extra OpenAI-compatible provider".to_string()],
        },
    ]
}

pub fn apply_provider_preset(config: &mut AppConfig, id: &str) -> Result<ProviderProfile, String> {
    let id = normalize_provider_id(id);
    let profile = available_providers(config)
        .into_iter()
        .find(|profile| normalize_provider_id(&profile.id) == id)
        .ok_or_else(|| format!("unknown provider: {id}"))?;
    config.provider = profile.id.clone();
    config.base_url = profile.base_url.clone();
    config.model = profile.default_model.clone();
    config.api_key_env = profile.api_key_env.clone();
    config.api_key = None;
    config.wire_session_token = None;
    config.account_id = None;
    config.account_name = None;
    config.account_email = None;
    config.protocol = profile.protocol;
    Ok(profile)
}

pub fn active_provider(config: &AppConfig) -> ProviderProfile {
    if let Some(mut profile) = available_providers(config).into_iter().find(|profile| {
        normalize_provider_id(&profile.id) == normalize_provider_id(&config.provider)
    }) {
        if !config.base_url.trim().is_empty() {
            profile.base_url = config.base_url.clone();
        }
        if !config.model.trim().is_empty() {
            profile.default_model = config.model.clone();
            if !profile.models.iter().any(|model| model == &config.model) {
                profile.models.insert(0, config.model.clone());
            }
        }
        if config.api_key_env.is_some() {
            profile.api_key_env = config.api_key_env.clone();
        }
        profile.protocol = config.protocol;
        return profile;
    }

    ProviderProfile {
        id: if config.provider.trim().is_empty() {
            "custom".to_string()
        } else {
            normalize_provider_id(&config.provider)
        },
        label: if config.provider.trim().is_empty() {
            "Custom OpenAI-compatible".to_string()
        } else {
            config.provider.clone()
        },
        base_url: config.base_url.clone(),
        default_model: config.model.clone(),
        models: if config.model.trim().is_empty() {
            Vec::new()
        } else {
            vec![config.model.clone()]
        },
        api_key_env: config.api_key_env.clone(),
        health: infer_health(&config.base_url),
        response_state: ResponseState::Stateless,
        protocol: config.protocol,
        supports_prompt_cache: false,
        supports_parallel_tool_calls: true,
        tool_calling_mode: ToolCallingMode::Native,
        notes: vec!["custom provider from config".to_string()],
    }
}

fn custom_provider_profile(custom: &CustomModelProvider) -> ProviderProfile {
    let default_model = custom.models.first().cloned().unwrap_or_default();
    ProviderProfile {
        id: custom.id.clone(),
        label: custom
            .name
            .clone()
            .unwrap_or_else(|| label_from_provider_id(&custom.id)),
        base_url: custom.base_url.clone(),
        default_model,
        models: custom.models.clone(),
        api_key_env: custom.api_key_env.clone(),
        health: infer_health(&custom.base_url),
        response_state: if custom.protocol == ProviderProtocol::Responses {
            ResponseState::Stateful
        } else {
            ResponseState::Stateless
        },
        protocol: custom.protocol,
        supports_prompt_cache: false,
        supports_parallel_tool_calls: true,
        tool_calling_mode: ToolCallingMode::Native,
        notes: vec!["custom provider from config.toml".to_string()],
    }
}

fn label_from_provider_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn model_namespace(model: &str) -> Option<&str> {
    let model = model.trim();
    if !model.contains('/') {
        return None;
    }
    let mut parts = model.split('/');
    let namespace = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if namespace.is_empty() || name.is_empty() {
        None
    } else {
        Some(namespace)
    }
}

pub fn provider_uses_health_endpoint(config: &AppConfig) -> bool {
    active_provider(config).health == ProviderHealth::HealthEndpoint
}

pub fn provider_is_stateless(_config: &AppConfig) -> bool {
    active_provider(_config).response_state == ResponseState::Stateless
}

pub fn provider_protocol(config: &AppConfig) -> ProviderProtocol {
    config.protocol
}

pub fn provider_headers(config: &AppConfig) -> Result<HeaderMap, String> {
    let profile = active_provider(config);
    let mut headers = HeaderMap::new();

    let api_key = if let Some(key) = config.api_key.as_deref() {
        key.to_string()
    } else if let Some(api_key_env) = profile.api_key_env.as_deref() {
        env::var(api_key_env).map_err(|_| {
            format!(
                "provider `{}` requires environment variable `{}` or configured api_key",
                profile.id, api_key_env
            )
        })?
    } else {
        String::new()
    };

    if !api_key.is_empty() {
        if profile.id == "anthropic" || profile.protocol == ProviderProtocol::AnthropicMessages {
            let value =
                HeaderValue::from_str(&api_key).map_err(|_| "invalid API key value".to_string())?;
            headers.insert("x-api-key", value);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|_| "invalid API key value".to_string())?;
            headers.insert(AUTHORIZATION, value);
        }
    }

    if profile.id == "openrouter" {
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/wirecli/wirecli"),
        );
        headers.insert("X-OpenRouter-Title", HeaderValue::from_static("Wire CLI"));
    }

    Ok(headers)
}

fn infer_health(base_url: &str) -> ProviderHealth {
    if base_url.contains("127.0.0.1") || base_url.contains("localhost") {
        ProviderHealth::HealthEndpoint
    } else {
        ProviderHealth::ModelsEndpoint
    }
}

fn normalize_provider_id(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "anthropic".to_string(),
        "glm" | "glm-5.1" | "z.ai" | "zai-glm" => "zai".to_string(),
        "gemini" => "google".to_string(),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{provider_model_mismatch, ProviderProtocol};
    use crate::config::{
        AppConfig, AppFeatures, CustomModelProvider, FeatureContextConfig, PermissionMode,
    };

    #[test]
    fn non_openrouter_provider_rejects_openrouter_catalog_model() {
        let mut config = AppConfig::default();
        config.provider = "qwenproxy".to_string();
        config.base_url = "http://127.0.0.1:3000/v1".to_string();
        config.model = "openrouter/free".to_string();
        config.api_key = Some("local-test-key".to_string());

        let message = provider_model_mismatch(&config).unwrap();

        assert!(message.contains("OpenRouter catalog id"));
        assert!(message.contains("qwenproxy"));
    }

    #[test]
    fn custom_provider_accepts_listed_model() {
        let config = AppConfig {
            provider: "qwenproxy".to_string(),
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            model: "qwen3.7-max".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some("local-test-key".to_string()),
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: AppFeatures::default(),
            feature_context: FeatureContextConfig::default(),
            model_providers: vec![CustomModelProvider {
                id: "qwenproxy".to_string(),
                name: Some("Qwen Proxy".to_string()),
                base_url: "http://127.0.0.1:3000/v1".to_string(),
                models: vec!["qwen3.7-max".to_string()],
                api_key_env: None,
                protocol: ProviderProtocol::ChatCompletions,
            }],
        };

        assert!(provider_model_mismatch(&config).is_none());
    }

    #[test]
    fn custom_provider_without_catalog_allows_namespaced_router_models() {
        let config = AppConfig {
            provider: "local-router".to_string(),
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            model: "anthropic/claude-sonnet-4.5".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some("local-test-key".to_string()),
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: AppFeatures::default(),
            feature_context: FeatureContextConfig::default(),
            model_providers: vec![CustomModelProvider {
                id: "local-router".to_string(),
                name: Some("Local Router".to_string()),
                base_url: "http://127.0.0.1:3000/v1".to_string(),
                models: Vec::new(),
                api_key_env: None,
                protocol: ProviderProtocol::ChatCompletions,
            }],
        };

        assert!(provider_model_mismatch(&config).is_none());
    }
}
