use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use gray_core::credential::{CredentialMaterial, SecretMap};
use gray_plugin::{ProviderRpcError, ProviderRpcFailure};
use serde::Deserialize;
use url::Url;
use zeroize::Zeroizing;

pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const PLAN_USAGE_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const AGENT_NAME: &str = "Gray";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const PREFERRED_PORT: u16 = 1455;
pub const LOGIN_TTL_SECS: u64 = 300;
pub const AUTH_BODY_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: Url,
    pub token: Url,
    pub revoke: Url,
    pub jwks: Url,
    pub resource: Url,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            issuer: ISSUER.to_string(),
            authorize: "https://auth.openai.com/api/accounts/authorize"
                .parse()
                .expect("authorize URL"),
            token: "https://auth.openai.com/api/accounts/oauth/token"
                .parse()
                .expect("token URL"),
            revoke: "https://auth.openai.com/api/accounts/oauth/revoke"
                .parse()
                .expect("revoke URL"),
            jwks: "https://auth.openai.com/.well-known/jwks.json"
                .parse()
                .expect("JWKS URL"),
            resource: RESOURCE.parse().expect("resource URL"),
        }
    }
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("gray-chatgpt/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn random_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub struct Attempt {
    pub state: Zeroizing<String>,
    pub nonce: Zeroizing<String>,
    pub verifier: Zeroizing<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Url,
}

impl Attempt {
    pub fn new(client_id: Option<String>, port: u16) -> Self {
        Self {
            state: Zeroizing::new(random_token()),
            nonce: Zeroizing::new(random_token()),
            verifier: Zeroizing::new(random_token()),
            client_id,
            redirect_uri: redirect_uri_for(port),
        }
    }
}

pub fn redirect_uri_for(port: u16) -> Url {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
        .parse()
        .expect("loopback redirect URI")
}

pub fn authorization_url(
    endpoints: &Endpoints,
    attempt: &Attempt,
    host_id: &str,
    login_hint: Option<&str>,
) -> Url {
    let mut url = endpoints.authorize.clone();
    {
        let mut q = url.query_pairs_mut();
        match &attempt.client_id {
            Some(client_id) => {
                q.append_pair("client_id", client_id);
            }
            None => {
                q.append_pair("client_id", REGISTRATION_CLIENT_ID);
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
        }
        q.append_pair("ext_agent_host_id", host_id);
        if let Some(hint) = login_hint.filter(|_| attempt.client_id.is_some()) {
            q.append_pair("login_hint", hint);
        }
        q.append_pair("response_type", "code")
            .append_pair("redirect_uri", attempt.redirect_uri.as_str())
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", attempt.state.as_str())
            .append_pair("nonce", attempt.nonce.as_str())
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &pkce_challenge(attempt.verifier.as_str()));
    }
    url
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Callback {
    pub code: Option<String>,
    pub states: Vec<String>,
    pub client_id: Option<String>,
    pub error: Option<String>,
}

pub fn parse_callback(target: &str) -> Option<Callback> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != CALLBACK_PATH {
        return None;
    }
    let mut callback = Callback::default();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => callback.code = Some(value.into_owned()),
            "state" => callback.states.push(value.into_owned()),
            "client_id" => callback.client_id = Some(value.into_owned()),
            "error" => callback.error = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(callback)
}

pub fn accept_callback(
    attempt: &Attempt,
    callback: &Callback,
) -> Result<(String, String), ProviderRpcFailure> {
    if callback.states.len() != 1 || callback.states[0] != *attempt.state {
        return Err(failure(
            "invalid_state",
            "Sign-in response did not match this attempt.",
        ));
    }
    if let Some(error) = &callback.error {
        return Err(if error == "access_denied" {
            failure(
                "access_denied",
                "Sign-in was declined. Run /connect again to allow Gray to use your ChatGPT plan.",
            )
        } else {
            failure("authorization_failed", "ChatGPT sign-in failed.")
        });
    }
    let code = callback
        .code
        .clone()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| failure("authorization_failed", "Sign-in returned no code."))?;
    let client_id = match &attempt.client_id {
        Some(saved) => match &callback.client_id {
            None => saved.clone(),
            Some(returned) if returned == saved => saved.clone(),
            Some(_) => {
                return Err(failure(
                    "client_mismatch",
                    "Sign-in returned a different app registration; not replacing the saved one.",
                ));
            }
        },
        None => callback
            .client_id
            .clone()
            .filter(|id| !id.is_empty() && id != REGISTRATION_CLIENT_ID)
            .ok_or_else(|| {
                failure(
                    "registration_incomplete",
                    "Registration returned no client id.",
                )
            })?,
    };
    Ok((code, client_id))
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    pub fn scopes(&self) -> Vec<&str> {
        self.scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    Rejected(String),
    Unavailable,
}

pub async fn exchange_code(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    attempt: &Attempt,
    code: &str,
    client_id: &str,
) -> Result<TokenResponse, TokenError> {
    post_token(
        http,
        endpoints,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", attempt.verifier.as_str()),
            ("redirect_uri", attempt.redirect_uri.as_str()),
            ("resource", RESOURCE),
        ],
    )
    .await
}

pub async fn refresh(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, TokenError> {
    post_token(
        http,
        endpoints,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .await
}

pub async fn revoke(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), TokenError> {
    let response = http
        .post(endpoints.revoke.clone())
        .form(&[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(|_| TokenError::Unavailable)?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().is_server_error() {
        Err(TokenError::Unavailable)
    } else {
        Err(TokenError::Rejected(oauth_error_code(response).await))
    }
}

async fn post_token(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> Result<TokenResponse, TokenError> {
    let response = http
        .post(endpoints.token.clone())
        .header("Accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|_| TokenError::Unavailable)?;
    let status = response.status();
    if status.is_server_error() {
        return Err(TokenError::Unavailable);
    }
    if !status.is_success() {
        return Err(TokenError::Rejected(oauth_error_code(response).await));
    }
    let body = read_bounded(response, AUTH_BODY_LIMIT)
        .await
        .ok_or(TokenError::Unavailable)?;
    serde_json::from_slice(&body).map_err(|_| TokenError::Unavailable)
}

async fn oauth_error_code(response: reqwest::Response) -> String {
    let body = read_bounded(response, AUTH_BODY_LIMIT)
        .await
        .unwrap_or_default();
    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error").and_then(|e| {
                e.as_str()
                    .map(str::to_string)
                    .or_else(|| e.get("code").and_then(|c| c.as_str()).map(str::to_string))
            })
        })
        .filter(|code| code.len() <= 64 && code.bytes().all(|b| b.is_ascii_graphic()))
        .unwrap_or_else(|| "unknown_error".to_string())
}

pub async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            return None;
        }
    }
    Some(body)
}

pub fn refresh_failure(error: TokenError) -> ProviderRpcError {
    match error {
        TokenError::Unavailable => ProviderRpcError::Rpc(ProviderRpcFailure {
            code: "unavailable".to_string(),
            message: "ChatGPT sign-in service unavailable; will retry.".to_string(),
            retryable: true,
            terminal: false,
        }),
        TokenError::Rejected(code) => {
            let host_code = match code.as_str() {
                "refresh_token_reused" => "refresh_reuse",
                "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "invalid_client" => "invalid_grant",
                _ => "unavailable",
            };
            ProviderRpcError::Rpc(ProviderRpcFailure {
                code: host_code.to_string(),
                message: format!("ChatGPT refresh rejected ({code}); sign in again."),
                retryable: host_code == "unavailable",
                terminal: host_code != "unavailable",
            })
        }
    }
}

pub fn credential(
    tokens: &TokenResponse,
    client_id: &str,
    subject: &str,
    email: Option<&str>,
    previous_refresh: Option<&str>,
    previous_scope: Option<&str>,
) -> Result<CredentialMaterial, ProviderRpcFailure> {
    let refresh = tokens
        .refresh_token
        .as_deref()
        .or(previous_refresh)
        .ok_or_else(|| failure("invalid_grant", "ChatGPT returned no refresh token."))?;
    let mut secrets = SecretMap::default();
    secrets.insert("access_token", tokens.access_token.clone());
    secrets.insert("refresh_token", refresh.to_string());
    let mut metadata = BTreeMap::new();
    metadata.insert("client_id".to_string(), client_id.to_string());
    metadata.insert("subject".to_string(), subject.to_string());
    if let Some(email) = email {
        metadata.insert("email".to_string(), email.to_string());
    }
    let scope = tokens
        .scope
        .as_deref()
        .or(previous_scope)
        .unwrap_or_default();
    metadata.insert("scope".to_string(), scope.to_string());
    Ok(CredentialMaterial {
        secrets,
        metadata,
        expires_at: Some(now_secs().saturating_add(tokens.expires_in)),
    })
}

pub fn failure(code: &str, message: &str) -> ProviderRpcFailure {
    ProviderRpcFailure {
        code: code.to_string(),
        message: message.to_string(),
        retryable: false,
        terminal: true,
    }
}
