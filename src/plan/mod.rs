mod id_token;
mod login;
mod models;
mod oauth;
mod page;
mod state;
mod store;
mod usage;

use gray_plugin::{
    ProviderModelsRequest, ProviderRefreshRequest, ProviderRevokeRequest, ProviderRpcError,
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

pub fn has_plan_pool() -> bool {
    store::has_plan_pool()
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
        "provider/usage" => usage(ctx, params).await,
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

async fn usage(ctx: &Plan, params: Value) -> Result<Value, ProviderRpcError> {
    let request: ProviderModelsRequest =
        serde_json::from_value(params).map_err(|_| protocol("usage request"))?;
    ensure_provider(&request.provider, &request.auth_method)?;
    let access = request
        .credential
        .credential
        .secrets
        .get("access_token")
        .ok_or_else(|| protocol("credential has no access token"))?;
    let account = request
        .credential
        .credential
        .metadata
        .get("account_id")
        .cloned();
    let email = request.credential.credential.metadata.get("email").cloned();
    if let Some((codex_tok, codex_acct)) = store::find_codex_cli_token("default", email.as_deref())
        && let Ok(mut card) = usage::fetch(&ctx.http, &codex_tok, codex_acct.as_deref()).await
    {
        if let Some(e) = email {
            card["email"] = json!(e);
        }
        return Ok(card);
    }
    usage::fetch(&ctx.http, access, account.as_deref()).await
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
    let status = match oauth::revoke(&ctx.http, &ctx.endpoints, client_id, refresh_token).await {
        Ok(()) => "revoked",
        Err(TokenError::Rejected(_)) => "revoked",
        Err(TokenError::Unavailable) => "local_only",
    };
    Ok(json!({
        "status": status,
        "message": "Removed local ChatGPT credentials.",
    }))
}

pub fn command(args: &[String]) -> Result<String, String> {
    let json_mode = args.iter().any(|a| a == "--json");
    let rest: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--json" && a.as_str() != "--fresh")
        .cloned()
        .collect();
    match rest.first().map(String::as_str) {
        Some("list") | Some("status") | None => Ok(list_out(json_mode)),
        Some("usage") => Ok(usage_json(rest.get(1).map(String::as_str))),
        Some("add") | Some("login") => {
            let label = rest
                .get(1)
                .map(String::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "account add needs a label".to_string())?;
            add_interactive(label, json_mode)
        }
        Some("use") => {
            let arg = rest.get(1).map(String::as_str);
            let want = match arg {
                None | Some("--clear") | Some("none") | Some("auto") => None,
                Some(l) => Some(l),
            };
            let sel = store::set_selected(want)?;
            if json_mode {
                Ok(json!({"ok": true, "selected": sel}).to_string())
            } else {
                Ok(match sel {
                    Some(l) => format!("serving account: {l}"),
                    None => "pin cleared".to_string(),
                })
            }
        }
        Some("remove") | Some("rm") => {
            let label = rest
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| "account remove needs a label".to_string())?;
            store::remove_account(label)?;
            if json_mode {
                Ok(json!({"ok": true, "message": format!("account {label:?} removed")}).to_string())
            } else {
                Ok(format!("removed account '{label}'"))
            }
        }
        Some("strategy") => {
            let next = rest.get(1).map(String::as_str).unwrap_or("fill_first");
            let set = store::set_strategy(next)?;
            if json_mode {
                Ok(json!({"ok": true, "strategy": set}).to_string())
            } else {
                Ok(format!("strategy: {set}"))
            }
        }
        Some(other) => Err(format!("unknown account verb {other:?}")),
    }
}

fn add_interactive(label: &str, json_mode: bool) -> Result<String, String> {
    let http = oauth::http_client().map_err(|e| e.to_string())?;
    let endpoints = Endpoints::default();
    let state = StateFile::default_location();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let material =
        runtime.block_on(login::interactive_login(&endpoints, &state, &http, label))?;
    let who = store::upsert_account(label, &material)?;
    if json_mode {
        Ok(json!({
            "ok": true,
            "label": label,
            "email": who,
            "message": format!("account {label:?} added ({who})"),
        })
        .to_string())
    } else {
        Ok(format!("Added ChatGPT login '{label}' ({who})"))
    }
}

pub fn list_out(json_mode: bool) -> String {
    let view = store::pool_view();
    if !json_mode {
        if view.accounts.is_empty() {
            return "no ChatGPT logins configured".to_string();
        }
        return view
            .accounts
            .iter()
            .map(|a| {
                let mark = if a.pinned { "*" } else { " " };
                let who = a.email.as_deref().unwrap_or("-");
                format!("{mark} {:<16} {who}", a.label)
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    list_json()
}

pub fn list_json() -> String {
    let view = store::pool_view();
    let accounts: Vec<Value> = view
        .accounts
        .iter()
        .map(|a| {
            json!({
                "label": a.label,
                "email": a.email,
                "pinned": a.pinned,
                "cooling": a.cooling,
            })
        })
        .collect();
    let serving = accounts
        .iter()
        .find(|a| a["pinned"] == true)
        .or_else(|| accounts.first())
        .and_then(|a| a["label"].as_str())
        .map(|l| json!({"kind": "account", "label": l}));
    let doc = json!({
        "version": 1,
        "plugin": "codex-sub",
        "provider": "chatgpt",
        "strategy": view.strategy,
        "selected": view.selected,
        "ambient": false,
        "serving": serving,
        "accounts": accounts,
    });
    doc.to_string()
}

pub fn usage_json(want: Option<&str>) -> String {
    let http = match oauth::http_client() {
        Ok(http) => http,
        Err(_) => return usage_doc(Vec::new()),
    };
    let endpoints = Endpoints::default();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return usage_doc(Vec::new()),
    };
    let entries: Vec<Value> = runtime.block_on(async {
        let mut entries = Vec::new();
        let now = oauth::now_secs();
        for mut account in store::accounts() {
            if want.is_some_and(|want| want != account.label) {
                continue;
            }
            if account.expires_at.is_some_and(|exp| exp <= now.saturating_add(60))
                && let (Some(cid), Some(rt)) = (
                    account.client_id.as_deref(),
                    account.refresh_token.as_deref(),
                )
                && let Ok(tokens) = oauth::refresh(&http, &endpoints, cid, rt).await
                && let Ok(mat) = oauth::credential(
                    &tokens,
                    cid,
                    account.subject.as_deref().unwrap_or_default(),
                    account.email.as_deref(),
                    Some(rt),
                    account.scope.as_deref(),
                )
            {
                let _ = store::update_credential(&account.label, &mat);
                if let Some(new_acc) = mat.secrets.get("access_token") {
                    account.access_token = new_acc.to_string();
                }
            }
            let outcome = fetch_account_usage(&http, &endpoints, &account).await;
            entries.push(match outcome {
                Ok(card) => json!({"label": account.label, "usage": card, "error": Value::Null}),
                Err(e) => {
                    json!({"label": account.label, "usage": Value::Null, "error": e.to_string()})
                }
            });
        }
        if entries.is_empty()
            && let Some(want) = want
        {
            entries.push(
                json!({"label": want, "usage": Value::Null, "error": "no such ChatGPT login"}),
            );
        }
        entries
    });
    usage_doc(entries)
}

async fn fetch_account_usage(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    account: &store::Account,
) -> Result<Value, ProviderRpcError> {
    if let Some((codex_tok, codex_acct)) =
        store::find_codex_cli_token(&account.label, account.email.as_deref())
        && let Ok(mut card) = usage::fetch(http, &codex_tok, codex_acct.as_deref()).await
    {
        if let Some(e) = &account.email {
            card["email"] = json!(e);
        }
        return Ok(card);
    }
    match usage::fetch(http, &account.access_token, account.account_id.as_deref()).await {
        Ok(mut card) => {
            if let Some(e) = &account.email {
                card["email"] = json!(e);
            }
            Ok(card)
        }
        Err(_) => {
            models::fetch(http, endpoints, &account.access_token).await?;
            Ok(json!({
                "available": true,
                "title": "ChatGPT",
                "plan": "Plan",
                "windows": [],
                "checked_at": usage::iso_now(),
                "email": account.email,
                "note": Value::Null,
            }))
        }
    }
}

fn usage_doc(entries: Vec<Value>) -> String {
    json!({
        "version": 1,
        "plugin": "codex-sub",
        "provider": "chatgpt",
        "accounts": entries,
    })
    .to_string()
}

fn auth_ref() -> &'static str {
    "plugin:codex-sub:chatgpt:chatgpt-plan"
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
    ProviderRpcError::Protocol(format!("chatgpt-plan: {message}"))
}
