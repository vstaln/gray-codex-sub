//! codex-sub: a protocol-1.2 sidecar for Gray.
//!
//! Unofficial Codex-backend route: logs in with the Codex CLI's own OAuth
//! client and sends Responses requests to `chatgpt.com/backend-api/codex`.
//! The official "Sign in with ChatGPT" provider is planned separately.

use codex_sub::{login, manifest, models, oauth};

use std::io::Write;

use gray_plugin::{
    ProviderAuthPoll, ProviderModelsRequest, ProviderRefreshRequest, ProviderRevokeRequest,
    ProviderRpcError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::login::LoginManager;
use crate::oauth::OAuthConfig;

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // `gray install plugin` registers sidecars by running `<bin> manifest`.
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", serde_json::to_string(&manifest::manifest())?);
        return Ok(());
    }
    let mut lines =
        tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(tokio::io::stdin()));
    let logins = LoginManager::default();
    let config = OAuthConfig::default();
    let http = oauth::http_client()?;
    let mut stdout = std::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let outcome = handle(&logins, &config, &http, &request).await;
        let response = match outcome {
            Ok(result) => Response {
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id: request.id,
                result: None,
                error: Some(error_value(error)),
            },
        };
        let frame = serde_json::to_string(&response)?;
        // Protocol frames only: never log credential payloads or upstream bodies.
        let _ = writeln!(stdout, "{frame}");
        stdout.flush()?;
        if request.method == "plugin/shutdown" {
            return Ok(());
        }
    }
    Ok(())
}

async fn handle(
    logins: &LoginManager,
    config: &OAuthConfig,
    http: &reqwest::Client,
    request: &Request,
) -> Result<Value, ProviderRpcError> {
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    match request.method.as_str() {
        "plugin/manifest" => Ok(serde_json::to_value(manifest::manifest()).unwrap()),
        "provider/auth/start" => {
            let provider = params
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let auth_method = params
                .get("auth_method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            ensure_provider(provider, auth_method)?;
            let start = logins.start(config).await?;
            Ok(serde_json::to_value(start).unwrap())
        }
        "provider/auth/poll" => {
            ensure_provider(
                params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                params
                    .get("auth_method")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?;
            let operation_id = params
                .get("operation_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(poll_value(logins.poll(operation_id).await))
        }
        "provider/auth/cancel" => {
            let operation_id = params
                .get("operation_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            logins.cancel(operation_id).await;
            Ok(json!({}))
        }
        "provider/auth/refresh" => {
            let request: ProviderRefreshRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider refresh request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            let material = oauth::refresh_material(config, &request.credential.credential).await?;
            Ok(serde_json::to_value(material).unwrap())
        }
        "provider/auth/revoke" => {
            let request: ProviderRevokeRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider revoke request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            Ok(json!({"status": "unsupported"}))
        }
        "provider/models" => {
            let request: ProviderModelsRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider models request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            let access = request
                .credential
                .credential
                .secrets
                .get("access_token")
                .ok_or_else(invalid_auth)?;
            let account_id = request
                .credential
                .credential
                .metadata
                .get("account_id")
                .ok_or_else(invalid_auth)?;
            let models = models::fetch_models(http, access, account_id).await?;
            Ok(serde_json::to_value(models).unwrap())
        }
        "plugin/shutdown" => Ok(json!({})),
        _ => Err(ProviderRpcError::Protocol(
            "unknown provider method".to_string(),
        )),
    }
}

fn ensure_provider(provider: &str, auth_method: &str) -> Result<(), ProviderRpcError> {
    if provider == manifest::PROVIDER_ID && auth_method == manifest::AUTH_METHOD_ID {
        Ok(())
    } else {
        Err(ProviderRpcError::Protocol(
            "unknown provider or auth method".to_string(),
        ))
    }
}

fn invalid_auth() -> ProviderRpcError {
    ProviderRpcError::Protocol("provider credential is incomplete".to_string())
}

fn invalid(message: &'static str) -> ProviderRpcError {
    ProviderRpcError::Protocol(message.to_string())
}

fn poll_value(poll: ProviderAuthPoll) -> Value {
    match poll {
        ProviderAuthPoll::Pending { retry_after_ms } => json!({
            "state": "pending",
            "retry_after_ms": retry_after_ms,
        }),
        ProviderAuthPoll::Completed(credential) => json!({
            "state": "completed",
            "credential": credential,
        }),
        ProviderAuthPoll::Failed(failure) => json!({
            "state": "failed",
            "error": failure,
        }),
        ProviderAuthPoll::Cancelled => json!({"state": "cancelled"}),
        ProviderAuthPoll::OperationLost => json!({"state": "operation_lost"}),
    }
}

fn error_value(error: ProviderRpcError) -> Value {
    match error {
        ProviderRpcError::Rpc(failure) => json!({
            "code": failure.code,
            "message": failure.message,
            "retryable": failure.retryable,
            "terminal": failure.terminal,
        }),
        ProviderRpcError::Protocol(message) => {
            json!({"code": "protocol", "message": message})
        }
        ProviderRpcError::Unavailable(message) => {
            json!({"code": "unavailable", "message": message})
        }
        ProviderRpcError::CapabilityMissing(message) => {
            json!({"code": "capability_missing", "message": message})
        }
    }
}
