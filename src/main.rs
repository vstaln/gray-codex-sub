//! codex-sub: a protocol-1.2 sidecar for Gray.
//!
//! Unofficial Codex-backend route: logs in with the Codex CLI's own OAuth
//! client, then serves chat through pooled `codex app-server` children
//! (one Codex thread per conversation) behind a per-turn loopback relay.

use codex_sub::{chat, login, manifest, models, oauth, relay, session, setup};

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
    session::install_oauth(config.clone());
    session::start_reaper();
    let relays: relay::Intents = Default::default();
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
        let outcome = handle(&logins, &config, &http, &relays, &request).await;
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
            session::shutdown_all();
            return Ok(());
        }
    }
    Ok(())
}

async fn handle(
    logins: &LoginManager,
    config: &OAuthConfig,
    http: &reqwest::Client,
    relays: &relay::Intents,
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
            let poll = logins.poll(operation_id).await;
            // A completed login is a credential sighting: mirror it so
            // `provider/chat` children can authenticate without the
            // credential ever crossing the wire again.
            if let ProviderAuthPoll::Completed(ref credential) = poll {
                setup::observe(credential);
            }
            Ok(poll_value(poll))
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
            // Mirror the sighting first: whichever way this grant lands,
            // the cache learns what the host last handed us.
            setup::observe(&request.credential.credential);
            match oauth::refresh_material(config, &request.credential.credential).await {
                Ok(material) => {
                    setup::observe(&material);
                    Ok(serde_json::to_value(material).unwrap())
                }
                // A session-driven refresh (the app-server's
                // `chatgptAuthTokens/refresh` answer) may have rotated the
                // refresh token past the copy the host is holding: retry
                // once with the cached token before reporting failure.
                Err(error) if is_invalid_grant(&error) => {
                    let cached_rt =
                        setup::read_cached()
                            .and_then(|c| c.refresh_token)
                            .filter(|rt| {
                                request.credential.credential.secrets.get("refresh_token")
                                    != Some(rt.as_str())
                            });
                    match cached_rt {
                        Some(rt) => {
                            let tokens = oauth::refresh_cached(config, &rt)
                                .await
                                .map_err(|_| error)?;
                            setup::store(&setup::CachedAuth {
                                access_token: tokens.access_token.clone(),
                                refresh_token: Some(tokens.refresh_token.clone()),
                                account_id: Some(tokens.account_id.clone()),
                                expires_at: Some(tokens.expires_at),
                                plan_type: tokens.plan_type.clone(),
                            });
                            let material = credential_from_tokens(&tokens);
                            Ok(serde_json::to_value(material).unwrap())
                        }
                        None => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        "provider/auth/revoke" => {
            let request: ProviderRevokeRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider revoke request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            // The login is gone: drop the mirror so children stop
            // authenticating with it.
            setup::clear();
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
            // Model fetches carry the credential too: keep the mirror warm.
            setup::observe(&request.credential.credential);
            Ok(serde_json::to_value(models).unwrap())
        }
        "provider/chat" => chat_turn(relays, params).await,
        "plugin/shutdown" => Ok(json!({})),
        _ => Err(ProviderRpcError::Protocol(
            "unknown provider method".to_string(),
        )),
    }
}

/// One relayed turn: park the intent, open the loopback relay, and hand
/// the host the relay URL + per-turn bearer its standard Responses POST
/// uses. The admitted POST translates, spawns or continues a pooled
/// `codex app-server` child, folds the thread's notifications into
/// Responses SSE and streams — the sidecar answers from the relay, never
/// from this method.
async fn chat_turn(relays: &relay::Intents, params: Value) -> Result<Value, ProviderRpcError> {
    let provider = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider != manifest::PROVIDER_ID {
        return Err(ProviderRpcError::Protocol("unknown provider".into()));
    }
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // Fail fast before opening anything: no binary, no credential, no relay.
    if setup::resolve_command().is_none() {
        return Err(ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()));
    }
    if setup::read_cached().is_none() {
        return Err(ProviderRpcError::Unavailable(setup::LOGIN_HINT.into()));
    }
    let bearer = format!("codex-sub-{}", chat::rand_hex(16));
    let intent = relay::RelayIntent {
        model: model.clone(),
    };
    relays
        .lock()
        .map(|mut r| {
            r.insert(bearer.clone(), intent);
        })
        .ok();
    let relays2 = relays.clone();
    let bearer2 = bearer.clone();
    let (port, _handle) =
        relay::start_turn_server(relays2, bearer2).map_err(ProviderRpcError::Unavailable)?;
    std::mem::forget(_handle);
    Ok(json!({
        "relay_url": format!("http://127.0.0.1:{port}/relay/{bearer}/responses"),
        "relay_token": bearer,
        "native_model": model,
    }))
}

fn is_invalid_grant(error: &ProviderRpcError) -> bool {
    matches!(error, ProviderRpcError::Rpc(f) if f.code == "invalid_grant")
}

/// CredentialMaterial the host expects from `auth/refresh`, built from a
/// cache-path token set.
fn credential_from_tokens(
    tokens: &oauth::CachedTokenSet,
) -> gray_core::credential::CredentialMaterial {
    let mut material = gray_core::credential::CredentialMaterial::default();
    material
        .secrets
        .insert("access_token", tokens.access_token.clone());
    material
        .secrets
        .insert("refresh_token", tokens.refresh_token.clone());
    material
        .metadata
        .insert("account_id".to_string(), tokens.account_id.clone());
    material.expires_at = Some(tokens.expires_at);
    material
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
