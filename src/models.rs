//! Bounded Codex model discovery.

use std::time::Duration;

use gray_plugin::{ProviderModel, ProviderModelCatalog, ProviderRpcError, ProviderRpcFailure};

const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const MAX_MODELS: usize = 128;
const MAX_BODY_BYTES: usize = 192 * 1024;

#[path = "models_tests.rs"]
#[cfg(test)]
mod tests;

fn unreachable() -> ProviderRpcError {
    ProviderRpcError::Rpc(ProviderRpcFailure {
        code: "unavailable".to_string(),
        message: "model discovery unavailable".to_string(),
        retryable: false,
        terminal: false,
    })
}

pub async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<ProviderModelCatalog, ProviderRpcError> {
    let response = client
        .get(MODELS_URL)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", account_id)
        .header("originator", "gray")
        .header("OpenAI-Beta", "responses=experimental")
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| unreachable())?;
    if response.status().is_redirection() {
        return Err(unreachable());
    }
    if !response.status().is_success() {
        return Err(unreachable());
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|_| unreachable())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY_BYTES {
            return Err(unreachable());
        }
    }
    parse_models(&body)
}

pub fn parse_models(body: &[u8]) -> Result<ProviderModelCatalog, ProviderRpcError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| unreachable())?;
    let mut models: Vec<ProviderModel> = Vec::new();
    if let Some(list) = value.get("models").and_then(|value| value.as_array()) {
        for item in list.iter().take(MAX_MODELS) {
            let visible =
                item.get("visibility").and_then(serde_json::Value::as_str) == Some("list");
            let supported = item
                .get("supported_in_api")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !visible || !supported {
                continue;
            }
            let Some(id) = item.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let model = ProviderModel {
                id: id.to_string(),
                name: item
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| item.get("name").and_then(serde_json::Value::as_str))
                    .unwrap_or(id)
                    .to_string(),
                context_window: Some(
                    item.get("context_window")
                        .and_then(serde_json::Value::as_u64)
                        .map(|value| value as u32)
                        .unwrap_or(0),
                ),
                reasoning_efforts: item
                    .get("reasoning_efforts")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            models.push(model);
        }
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(ProviderModelCatalog { models })
}
