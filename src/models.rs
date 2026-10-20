//! Model catalog from `codex app-server`'s `model/list`: the CLI's own
//! view of what the signed-in account may run. No list is hardcoded and
//! no borrowed token is sent — the app-server authenticates with the
//! CLI's stored login.
use crate::appserver;
use gray_plugin::{ProviderModel, ProviderModelCatalog, ProviderRpcError};
use serde_json::{Value, json};
/// Discovery is bounded: more rows than this means something is wrong.
const MAX_MODELS: usize = 128;
const PAGE_SIZE: u32 = 64;
#[path = "models_tests.rs"]
#[cfg(test)]
mod tests;
fn unavailable(message: impl Into<String>) -> ProviderRpcError {
    ProviderRpcError::Unavailable(message.into())
}
/// One catalog fetch: paged `model/list` over a single probe client.
pub fn fetch() -> Result<ProviderModelCatalog, ProviderRpcError> {
    let mut models = Vec::new();
    appserver::with_client(|client| {
        let mut cursor = Value::Null;
        loop {
            let resp = client.call(
                "model/list",
                json!({
                    "cursor": cursor,
                    "limit": PAGE_SIZE,
                    "includeHidden": false,
                }),
            )?;
            if let Some(data) = resp.get("data").and_then(Value::as_array) {
                for item in data {
                    if let Some(row) = model_row(item) {
                        models.push(row);
                        if models.len() >= MAX_MODELS {
                            return Ok(());
                        }
                    }
                }
            }
            match resp.get("nextCursor") {
                Some(c) if !c.is_null() => cursor = c.clone(),
                _ => return Ok(()),
            }
        }
    })
    .map_err(ProviderRpcError::from)?;
    if models.is_empty() {
        return Err(unavailable("codex app-server reported no models"));
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(ProviderModelCatalog { models })
}
/// Map one `Model` (v2 app-server schema): `model` is the slug
/// `thread/start` accepts, `displayName` the picker label. `hidden`
/// rows are server-flagged picker exclusions — skipped even when the
/// page asks for them. The schema carries no context-window field.
fn model_row(v: &Value) -> Option<ProviderModel> {
    if v.get("hidden").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let slug = v.get("model").and_then(Value::as_str)?.to_string();
    Some(ProviderModel {
        id: slug.clone(),
        name: v
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&slug)
            .to_string(),
        context_window: None,
        reasoning_efforts: v
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| {
                        o.get("reasoningEffort")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        variants: vec![],
        slots: vec![],
        price_cliff_tokens: None,
    })
}
