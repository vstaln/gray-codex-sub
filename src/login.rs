//! Host-visible login operation state.

use std::collections::HashMap;
use std::sync::Arc;

use gray_core::credential::CredentialMaterial;
use gray_plugin::{ProviderAuthPoll, ProviderAuthStart, ProviderRpcError, ProviderRpcFailure};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::oauth::{self, OAuthConfig};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum LoginOutcome {
    Completed(CredentialMaterial),
    Failed(ProviderRpcFailure),
}

struct OperationMeta {
    cancel: CancellationToken,
    created_at: u64,
}

/// One pending login. The callback task owns the result channel; poll never
/// performs OAuth work itself, so one large browser login cannot leak tokens.
pub struct LoginManager {
    results: Mutex<HashMap<String, Arc<Mutex<Option<LoginOutcome>>>>>,
    handles: Mutex<HashMap<String, JoinHandle<()>>>,
    meta: Mutex<HashMap<String, OperationMeta>>,
}

impl Default for LoginManager {
    fn default() -> Self {
        Self {
            results: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
        }
    }
}

impl LoginManager {
    pub async fn start(&self, config: &OAuthConfig) -> Result<ProviderAuthStart, ProviderRpcError> {
        let listener = oauth::bind_listener(config.callback_ports)
            .await
            .map_err(|_| ProviderRpcError::Unavailable("login callback unavailable".to_string()))?;
        let port = listener
            .local_addr()
            .map(|address| address.port())
            .map_err(|_| ProviderRpcError::Unavailable("login callback unavailable".to_string()))?;
        let redirect_uri = oauth::redirect_uri_for(port, &config.callback_path)
            .map_err(|_| ProviderRpcError::Protocol("invalid redirect URI".to_string()))?;
        let pkce = oauth::pkce().map_err(|_| ProviderRpcError::Protocol("PKCE".to_string()))?;
        let authorize = oauth::authorization_url(config, &redirect_uri, &pkce)
            .map_err(|_| ProviderRpcError::Protocol("authorization URL".to_string()))?;
        let operation_id = random_id();
        let result = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let callback_cancel = cancel.clone();
        let state = pkce.state.clone();
        let callback_path = config.callback_path.clone();
        let config = config.clone();
        let callback_result = result.clone();
        let handle = tokio::spawn(async move {
            match oauth::run_callback_once(
                listener,
                &callback_path,
                state.as_str(),
                callback_cancel,
            )
            .await
            {
                Ok(code) => {
                    let outcome =
                        oauth::exchange_authorization_code(&config, &code, &pkce, &redirect_uri)
                            .await
                            .map(LoginOutcome::Completed)
                            .unwrap_or_else(|error| LoginOutcome::Failed(rpc_failure(error)));
                    *callback_result.lock().await = Some(outcome);
                }
                Err(error) => {
                    if error.code == "cancelled" {
                        return;
                    }
                    let outcome = LoginOutcome::Failed(ProviderRpcFailure {
                        code: error.code,
                        message: "login failed".to_string(),
                        retryable: false,
                        terminal: true,
                    });
                    *callback_result.lock().await = Some(outcome);
                }
            }
        });
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default();
        self.results
            .lock()
            .await
            .insert(operation_id.clone(), result);
        self.handles
            .lock()
            .await
            .insert(operation_id.clone(), handle);
        self.meta
            .lock()
            .await
            .insert(operation_id.clone(), OperationMeta { cancel, created_at });
        Ok(ProviderAuthStart {
            operation_id,
            status: "pending".to_string(),
            verification_uri: authorize.to_string(),
            expires_at: created_at + oauth::LOGIN_TTL_SECS,
            retry_after_ms: 1_000,
        })
    }

    pub async fn poll(&self, operation_id: &str) -> ProviderAuthPoll {
        let Some(result) = self.results.lock().await.get(operation_id).cloned() else {
            return ProviderAuthPoll::OperationLost;
        };
        if let Some(outcome) = result.lock().await.clone() {
            return match outcome {
                LoginOutcome::Completed(material) => ProviderAuthPoll::Completed(material),
                LoginOutcome::Failed(failure) => ProviderAuthPoll::Failed(failure),
            };
        }
        let expired = self
            .meta
            .lock()
            .await
            .get(operation_id)
            .map(|meta| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_secs())
                    .unwrap_or_default()
                    > meta.created_at + oauth::LOGIN_TTL_SECS
            })
            .unwrap_or(false);
        if expired {
            ProviderAuthPoll::OperationLost
        } else {
            ProviderAuthPoll::Pending {
                retry_after_ms: Some(1_000),
            }
        }
    }

    pub async fn cancel(&self, operation_id: &str) {
        if let Some(meta) = self.meta.lock().await.remove(operation_id) {
            meta.cancel.cancel();
        }
        if let Some(handle) = self.handles.lock().await.remove(operation_id) {
            handle.abort();
        }
        self.results.lock().await.remove(operation_id);
    }
}

fn rpc_failure(error: ProviderRpcError) -> ProviderRpcFailure {
    match error {
        ProviderRpcError::Rpc(failure) => failure,
        _ => ProviderRpcFailure {
            code: "unavailable".to_string(),
            message: "provider login unavailable".to_string(),
            retryable: false,
            terminal: true,
        },
    }
}

fn random_id() -> String {
    use base64::Engine;
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(first.as_bytes());
    bytes.extend_from_slice(second.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
