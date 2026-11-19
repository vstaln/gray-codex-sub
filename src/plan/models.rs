use gray_plugin::{ProviderModel, ProviderModelCatalog, ProviderRpcError, ProviderRpcFailure};
use serde_json::Value;

use super::oauth::{Endpoints, read_bounded};

const MAX_MODELS: usize = 128;
const CATALOG_BODY_LIMIT: usize = 8 * 1024 * 1024;

pub async fn fetch(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<ProviderModelCatalog, ProviderRpcError> {
    let url = format!(
        "{}/models",
        endpoints.resource.as_str().trim_end_matches('/')
    );
    let response = http
        .get(url)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ProviderRpcError::Rpc(ProviderRpcFailure {
            code: "unauthorized".to_string(),
            message: "ChatGPT rejected this sign-in for model listing; run /connect again."
                .to_string(),
            retryable: false,
            terminal: true,
        }));
    }
    if !status.is_success() {
        return Err(unavailable());
    }
    let body = read_bounded(response, CATALOG_BODY_LIMIT)
        .await
        .ok_or_else(unavailable)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| unavailable())?;
    Ok(parse(&value))
}

pub fn parse(value: &Value) -> ProviderModelCatalog {
    let mut models: Vec<ProviderModel> = Vec::new();
    let entries = value
        .get("models")
        .or_else(|| value.get("data"))
        .and_then(Value::as_array);
    for item in entries.into_iter().flatten() {
        if models.len() >= MAX_MODELS {
            break;
        }
        if item.get("visibility").and_then(Value::as_str) != Some("list") {
            continue;
        }
        let Some(slug) = item
            .get("slug")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if models.iter().any(|m| m.id == slug) {
            continue;
        }
        models.push(ProviderModel {
            id: slug.to_string(),
            name: item
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(slug)
                .to_string(),
            context_window: item
                .get("context_window")
                .and_then(Value::as_u64)
                .and_then(|w| u32::try_from(w).ok()),
            reasoning_efforts: reasoning_efforts(item),
            price_cliff_tokens: None,
            variants: Vec::new(),
            slots: Vec::new(),
        });
    }
    ProviderModelCatalog { models }
}

fn reasoning_efforts(item: &Value) -> Vec<String> {
    [
        "supported_reasoning_levels",
        "reasoning_efforts",
        "supported_reasoning_efforts",
    ]
    .iter()
    .find_map(|key| item.get(*key).and_then(Value::as_array))
    .map(|levels| {
        levels
            .iter()
            .filter_map(|level| {
                level
                    .as_str()
                    .or_else(|| level.get("effort").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .collect()
    })
    .unwrap_or_default()
}

fn unavailable() -> ProviderRpcError {
    ProviderRpcError::Rpc(ProviderRpcFailure {
        code: "unavailable".to_string(),
        message: "ChatGPT model listing unavailable.".to_string(),
        retryable: true,
        terminal: false,
    })
}
