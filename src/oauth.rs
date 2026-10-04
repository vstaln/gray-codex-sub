//! Codex OAuth core: PKCE, loopback callback, token exchange, account claim.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use gray_core::credential::{CredentialMaterial, SecretMap};
use gray_plugin::{ProviderRpcError, ProviderRpcFailure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::Zeroizing;

pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const LOGIN_TTL_SECS: u64 = 300;
const MAX_HTTP_HEAD_BYTES: usize = 8 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 192 * 1024;

/// Production configuration. `issuer` remains injectable only for loopback tests.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub client_id: String,
    pub callback_path: String,
    pub callback_ports: [u16; 2],
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_id: CLIENT_ID.to_string(),
            callback_path: CALLBACK_PATH.to_string(),
            callback_ports: [1455, 1457],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pkce {
    pub state: Zeroizing<String>,
    pub verifier: Zeroizing<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: SecretStringValue,
    #[serde(default)]
    pub refresh_token: Option<SecretStringValue>,
    pub expires_in: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretStringValue(String);

impl SecretStringValue {
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TokenResponse {
    pub fn access_token(&self) -> &str {
        self.access_token.as_str()
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_ref().map(SecretStringValue::as_str)
    }
}

pub fn pkce() -> Result<Pkce> {
    let state = random_unpadded(32);
    let verifier = random_unpadded(32);
    Ok(Pkce {
        state: Zeroizing::new(state),
        verifier: Zeroizing::new(verifier),
    })
}

pub fn pkce_challenge(verifier: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest))
}

fn random_unpadded(bytes: usize) -> String {
    use base64::Engine;
    let mut buffer = Vec::with_capacity(bytes);
    while buffer.len() < bytes {
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        buffer.extend_from_slice(first.as_bytes());
        if buffer.len() < bytes {
            buffer.extend_from_slice(second.as_bytes());
        }
    }
    buffer.truncate(bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn safe_header(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_control() && byte != 0x7f)
}

pub fn account_id_from_access_token(access_token: &str) -> Result<String, ProviderRpcError> {
    let mut segments = access_token.split('.');
    segments.next();
    let payload = segments
        .next()
        .ok_or_else(|| token_failure("malformed account claim"))?;
    segments
        .next()
        .ok_or_else(|| token_failure("malformed account claim"))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| token_failure("malformed account claim"))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|_| token_failure("malformed account claim"))?;
    let account_id = claims
        .get("https://api.openai.com/auth")
        .and_then(|outer| outer.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| safe_header(value))
        .ok_or_else(|| token_failure("malformed account claim"))?;
    Ok(account_id.to_string())
}

fn token_failure(code: &'static str) -> ProviderRpcError {
    ProviderRpcError::Rpc(ProviderRpcFailure {
        code: code.to_string(),
        message: "login failed".to_string(),
        retryable: false,
        terminal: true,
    })
}

fn rpc_unavailable() -> ProviderRpcError {
    ProviderRpcError::Unavailable("provider login unavailable".to_string())
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .context("login HTTP client")
}

pub fn redirect_uri_for(port: u16, callback_path: &str) -> Result<Url> {
    Url::parse(&format!("http://127.0.0.1:{port}{callback_path}")).context("redirect URI")
}

pub fn authorization_url(config: &OAuthConfig, redirect_uri: &Url, pkce: &Pkce) -> Result<Url> {
    let mut url = Url::parse(AUTHORIZE_URL).context("authorization URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", "")
        .append_pair("state", pkce.state.as_str())
        .append_pair("code_challenge", pkce.challenge()?.as_str())
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "gray");
    Ok(url)
}

impl Pkce {
    pub fn challenge(&self) -> Result<String> {
        pkce_challenge(self.verifier.as_str())
    }
}

#[derive(Debug)]
pub struct CallbackError {
    pub code: String,
}

pub async fn exchange_authorization_code(
    config: &OAuthConfig,
    code: &str,
    pkce: &Pkce,
    redirect_uri: &Url,
) -> Result<CredentialMaterial, ProviderRpcError> {
    let client = http_client().map_err(|_| rpc_unavailable())?;
    let form = [
        ("grant_type", "authorization_code".to_string()),
        ("client_id", config.client_id.clone()),
        ("code", code.to_string()),
        ("code_verifier", pkce.verifier.as_str().to_string()),
        ("redirect_uri", redirect_uri.as_str().to_string()),
    ];
    let token = post_token(&client, &form)
        .await
        .map_err(|error| match error.code.as_str() {
            "access_denied" => token_failure("access_denied"),
            _ => rpc_unavailable(),
        })?;
    material_from_token(token)
}

pub async fn refresh_material(
    config: &OAuthConfig,
    current: &CredentialMaterial,
) -> Result<CredentialMaterial, ProviderRpcError> {
    let refresh_token = current
        .secrets
        .get("refresh_token")
        .ok_or_else(|| token_failure("invalid_grant"))?;
    let client = http_client().map_err(|_| rpc_unavailable())?;
    let form = [
        ("grant_type", "refresh_token".to_string()),
        ("client_id", config.client_id.clone()),
        ("refresh_token", refresh_token.to_string()),
    ];
    let token = post_token(&client, &form)
        .await
        .map_err(|error| match error.code.as_str() {
            "invalid_grant" => token_failure("invalid_grant"),
            _ => rpc_unavailable(),
        })?;
    let mut material = material_from_token(token)?;
    if material.secrets.get("refresh_token").is_none() {
        material
            .secrets
            .insert("refresh_token", refresh_token.to_string());
    }
    let old_account = current.metadata.get("account_id");
    let new_account = material.metadata.get("account_id");
    if old_account != new_account {
        return Err(token_failure("account_changed"));
    }
    material
        .expires_at
        .take_if(|value| *value > now_secs())
        .ok_or_else(|| token_failure("invalid_grant"))?;
    Ok(material)
}

fn material_from_token(token: TokenResponse) -> Result<CredentialMaterial, ProviderRpcError> {
    let account_id = account_id_from_access_token(token.access_token())?;
    let expires_at = now_secs()
        .checked_add(token.expires_in as u64)
        .ok_or_else(|| token_failure("invalid_grant"))?;
    let mut material = CredentialMaterial {
        secrets: SecretMap::default(),
        metadata: BTreeMap::new(),
        expires_at: Some(expires_at),
    };
    material
        .secrets
        .insert("access_token", token.access_token().to_string());
    if let Some(refresh) = token.refresh_token() {
        material
            .secrets
            .insert("refresh_token", refresh.to_string());
    }
    material
        .metadata
        .insert("account_id".to_string(), account_id);
    Ok(material)
}

async fn post_token(
    client: &reqwest::Client,
    form: &[(&str, String)],
) -> std::result::Result<TokenResponse, CallbackError> {
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(form)
        .send()
        .await
        .map_err(|_| CallbackError {
            code: "unavailable".to_string(),
        })?;
    if !response.status().is_success() {
        let code = response
            .headers()
            .get("x-codex-error")
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
            .unwrap_or_else(|| "unavailable".to_string());
        return Err(CallbackError { code });
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|_| CallbackError {
        code: "unavailable".to_string(),
    })? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_HTTP_BODY_BYTES {
            return Err(CallbackError {
                code: "unavailable".to_string(),
            });
        }
    }
    serde_json::from_slice(&body).map_err(|_| CallbackError {
        code: "unavailable".to_string(),
    })
}

pub async fn bind_listener(ports: [u16; 2]) -> Result<TcpListener> {
    for port in ports {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(_) => continue,
        }
    }
    anyhow::bail!("no callback port available")
}

pub async fn run_callback_once(
    listener: TcpListener,
    callback_path: &str,
    state: &str,
    cancel: CancellationToken,
) -> std::result::Result<String, CallbackError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(CallbackError { code: "cancelled".to_string() }),
        accepted = listener.accept() => {
            let (mut socket, _) = accepted.map_err(|_| CallbackError { code: "unavailable".to_string() })?;
            let (head, _body) = read_request(&mut socket).await.map_err(|_| CallbackError { code: "invalid_request".to_string() })?;
            let target = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .ok_or(CallbackError { code: "invalid_request".to_string() })?;
            let url = Url::parse(&format!("http://127.0.0.1{target}"))
                .map_err(|_| CallbackError { code: "invalid_request".to_string() })?;
            if url.path() != callback_path || url.fragment().is_some() {
                let _ = write_html(&mut socket, "Login failed.").await;
                return Err(CallbackError { code: "invalid_request".to_string() });
            }
            let mut code = None;
            let mut states = Vec::new();
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "code" => code = Some(value.into_owned()),
                    "state" => states.push(value.into_owned()),
                    _ => {}
                }
            }
            let Some(code) = code.filter(|_| states.len() == 1 && states[0] == state) else {
                write_html(&mut socket, "Login failed.").await.ok();
                return Err(CallbackError {
                    code: "invalid_request".to_string(),
                });
            };
            write_html(&mut socket, "Login complete. You can close this window.").await.map_err(|_| CallbackError { code: "unavailable".to_string() })?;
            Ok(code)
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<(String, String)> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.len() > MAX_HTTP_HEAD_BYTES {
            anyhow::bail!("request head too large");
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let separator = head.find("\r\n\r\n").map(|index| index + 4);
    let body = separator
        .map(|index| head[index..].to_string())
        .unwrap_or_default();
    Ok((head, body))
}

async fn write_html(socket: &mut TcpStream, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

#[path = "oauth_tests.rs"]
#[cfg(test)]
mod tests;
