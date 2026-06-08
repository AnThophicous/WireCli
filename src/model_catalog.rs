use crate::config::AppConfig;
use crate::models::{endpoint_error_message, find_model, parse_models, ModelInfo};
use crate::providers::provider_headers;
use reqwest::header::HeaderMap;
use std::time::Duration;

pub async fn load_models(config: &AppConfig) -> Result<Vec<ModelInfo>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let headers = provider_headers(config)?;
    let value = get_models_async(&client, config, headers).await?;
    Ok(parse_models(&value, &config.model))
}

pub fn load_models_blocking(
    config: &AppConfig,
    timeout: Duration,
) -> Result<Vec<ModelInfo>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let headers = provider_headers(config)?;
    let value = get_models_blocking(&client, config, headers)?;
    Ok(parse_models(&value, &config.model))
}

pub fn current_model_info(models: &[ModelInfo], model_id: &str) -> Option<ModelInfo> {
    find_model(models, model_id).cloned()
}

async fn get_models_async(
    client: &reqwest::Client,
    config: &AppConfig,
    headers: HeaderMap,
) -> Result<serde_json::Value, String> {
    let response = client
        .get(format!("{}/models", config.base_url.trim_end_matches('/')))
        .headers(headers)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(endpoint_error_message(
            "models endpoint",
            status.as_u16(),
            &value,
            "login required",
        ));
    }
    Ok(value)
}

fn get_models_blocking(
    client: &reqwest::blocking::Client,
    config: &AppConfig,
    headers: HeaderMap,
) -> Result<serde_json::Value, String> {
    let response = client
        .get(format!("{}/models", config.base_url.trim_end_matches('/')))
        .headers(headers)
        .send()
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(endpoint_error_message(
            "models endpoint",
            status.as_u16(),
            &value,
            "login required",
        ));
    }
    Ok(value)
}

pub fn health_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

pub fn health_is_connected(value: &serde_json::Value) -> bool {
    value
        .get("status")
        .and_then(|v| v.as_str())
        .map(|status| status == "ok")
        .unwrap_or(false)
        || value
            .pointer("/metrics/cache/connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{health_base_url, health_is_connected};

    #[test]
    fn health_base_url_keeps_v1_suffix_for_wire_gateway() {
        assert_eq!(
            health_base_url("http://127.0.0.1:3000/v1"),
            "http://127.0.0.1:3000/v1"
        );
    }

    #[test]
    fn health_accepts_connected_cache_payload() {
        let value = serde_json::json!({
            "metrics": { "cache": { "connected": true } }
        });
        assert!(health_is_connected(&value));
    }
}
