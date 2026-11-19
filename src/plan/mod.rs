mod id_token;
mod login;
mod models;
mod oauth;
mod page;
mod state;

use gray_plugin::{
    ProviderModelsRequest, ProviderRefreshRequest, ProviderRevokeRequest, ProviderRpcError,
    ProviderRpcFailure,
};
use login::LoginManager;
use oauth::{Endpoints, TokenError};
use serde_json::{Value, json};
use state::StateFile;

pub(crate) struct Plan {
    logins: LoginManager,
    endpoints: Endpoints,
    http: reqwest::Client,
}

pub(crate) fn forget_registration() -> std::io::Result<()> {
    StateFile::default_location().forget_registration()
}

impl Plan {
    pub(crate) fn new() -> Result<Self, ProviderRpcError> {
        let http = oauth::http_client()
            .map_err(|_| ProviderRpcError::Unavailable("ChatGPT HTTP client unavailable".into()))?;
        let endpoints = Endpoints::default();
        Ok(Self {
            logins: LoginManager::new(
                endpoints.clone(),
                StateFile::default_location(),
                http.clone(),
            ),
            endpoints,
            http,
        })
    }

    pub(crate) async fn handle(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ProviderRpcError> {
        handle(self, method, params).await
    }
}
async fn handle(ctx: &Plan, method: &str, params: Value) -> Result<Value, ProviderRpcError> {
    match method {
        "provider/auth/start" => {
            ensure_provider(
                str_param(&params, "provider"),
                str_param(&params, "auth_method"),
            )?;
            Ok(serde_json::to_value(ctx.logins.start().await?).unwrap())
        }
        "provider/auth/poll" => Ok(super::poll_value(
            ctx.logins.poll(str_param(&params, "operation_id")).await,
        )),
        "provider/auth/cancel" => {
            ctx.logins.cancel(str_param(&params, "operation_id")).await;
            Ok(json!({}))
        }
        "provider/auth/refresh" => refresh(ctx, params).await,
        "provider/auth/revoke" => revoke(ctx, params).await,
        "provider/models" => {
            let request: ProviderModelsRequest =
                serde_json::from_value(params).map_err(|_| protocol("models request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            let access = request
                .credential
                .credential
                .secrets
                .get("access_token")
                .ok_or_else(|| protocol("credential has no access token"))?;
            let catalog = models::fetch(&ctx.http, &ctx.endpoints, access).await?;
            Ok(serde_json::to_value(catalog).unwrap())
        }
        _ => Err(protocol("unknown method")),
    }
}

async fn refresh(ctx: &Plan, params: Value) -> Result<Value, ProviderRpcError> {
    let request: ProviderRefreshRequest =
        serde_json::from_value(params).map_err(|_| protocol("refresh request"))?;
    ensure_provider(&request.provider, &request.auth_method)?;
    let current = &request.credential.credential;
    let (Some(client_id), Some(refresh_token)) = (
        current.metadata.get("client_id"),
        current.secrets.get("refresh_token"),
    ) else {
        return Err(ProviderRpcError::Rpc(oauth::failure(
            "invalid_grant",
            "Stored ChatGPT sign-in is incomplete; sign in again.",
        )));
    };
    let tokens = oauth::refresh(&ctx.http, &ctx.endpoints, client_id, refresh_token)
        .await
        .map_err(oauth::refresh_failure)?;
    let material = oauth::credential(
        &tokens,
        client_id,
        current
            .metadata
            .get("subject")
            .map(String::as_str)
            .unwrap_or_default(),
        current.metadata.get("email").map(String::as_str),
        Some(refresh_token),
        current.metadata.get("scope").map(String::as_str),
    )
    .map_err(ProviderRpcError::Rpc)?;
    Ok(serde_json::to_value(material).unwrap())
}

async fn revoke(ctx: &Plan, params: Value) -> Result<Value, ProviderRpcError> {
    let request: ProviderRevokeRequest =
        serde_json::from_value(params).map_err(|_| protocol("revoke request"))?;
    ensure_provider(&request.provider, &request.auth_method)?;
    let current = &request.credential.credential;
    let (Some(client_id), Some(refresh_token)) = (
        current.metadata.get("client_id"),
        current.secrets.get("refresh_token"),
    ) else {
        return Ok(json!({"status": "revoked"}));
    };
    for delay_ms in [0u64, 500, 1500] {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        match oauth::revoke(&ctx.http, &ctx.endpoints, client_id, refresh_token).await {
            Ok(()) => return Ok(json!({"status": "revoked"})),
            Err(TokenError::Unavailable) => continue,
            Err(TokenError::Rejected(_)) => break,
        }
    }
    Err(ProviderRpcError::Rpc(ProviderRpcFailure {
        code: "revocation_unconfirmed".to_string(),
        message: "Signed out locally, but OpenAI did not confirm revocation. \
                  Remove Gray under ChatGPT Settings → Apps to cut it off."
            .to_string(),
        retryable: true,
        terminal: false,
    }))
}

fn str_param<'a>(params: &'a Value, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn ensure_provider(provider: &str, auth_method: &str) -> Result<(), ProviderRpcError> {
    if provider == "chatgpt" && auth_method == "chatgpt-plan" {
        Ok(())
    } else {
        Err(protocol("unknown provider or auth method"))
    }
}

fn protocol(message: &str) -> ProviderRpcError {
    ProviderRpcError::Protocol(message.to_string())
}
