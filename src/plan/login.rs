use std::collections::HashMap;
use std::sync::Arc;

use gray_core::credential::CredentialMaterial;
use gray_plugin::{ProviderAuthPoll, ProviderAuthStart, ProviderRpcError, ProviderRpcFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::id_token;
use super::oauth::{self, Attempt, Endpoints, TokenError, failure};
use super::page;
use super::state::{Registration, StateFile};

const MAX_HEAD_BYTES: usize = 8 * 1024;

#[derive(Clone)]
enum Outcome {
    Completed(CredentialMaterial),
    Failed(ProviderRpcFailure),
}

struct Operation {
    outcome: Arc<Mutex<Option<Outcome>>>,
    cancel: CancellationToken,
    expires_at: u64,
}

pub struct LoginManager {
    endpoints: Endpoints,
    state: StateFile,
    http: reqwest::Client,
    operations: Mutex<HashMap<String, Operation>>,
}

impl LoginManager {
    pub fn new(endpoints: Endpoints, state: StateFile, http: reqwest::Client) -> Self {
        Self {
            endpoints,
            state,
            http,
            operations: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start(&self) -> Result<ProviderAuthStart, ProviderRpcError> {
        let stale: Vec<String> = self.operations.lock().await.keys().cloned().collect();
        for id in stale {
            self.cancel(&id).await;
        }
        let host_id = self
            .state
            .host_id()
            .map_err(|_| ProviderRpcError::Unavailable("cannot save Gray's host id".into()))?;
        let registration = self.state.registration();
        let listener = bind_loopback().await?;
        let port = listener
            .local_addr()
            .map_err(|_| ProviderRpcError::Unavailable("login callback unavailable".into()))?
            .port();
        let attempt = Attempt::new(registration.as_ref().map(|r| r.client_id.clone()), port);
        let url = oauth::authorization_url(
            &self.endpoints,
            &attempt,
            &host_id,
            registration.as_ref().and_then(|r| r.email.as_deref()),
        );
        let operation_id = oauth::random_token();
        let outcome = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let expires_at = oauth::now_secs() + oauth::LOGIN_TTL_SECS;
        let flow = Flow {
            endpoints: self.endpoints.clone(),
            state: self.state.clone(),
            http: self.http.clone(),
            attempt,
            saved: registration,
        };
        let task_outcome = outcome.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let deadline =
                tokio::time::sleep(std::time::Duration::from_secs(oauth::LOGIN_TTL_SECS));
            let result = tokio::select! {
                _ = task_cancel.cancelled() => return,
                _ = deadline => Outcome::Failed(failure("expired", "Sign-in timed out.")),
                result = flow.run(listener) => match result {
                    Ok(material) => Outcome::Completed(material),
                    Err(failure) => Outcome::Failed(failure),
                },
            };
            *task_outcome.lock().await = Some(result);
        });
        self.operations.lock().await.insert(
            operation_id.clone(),
            Operation {
                outcome,
                cancel,
                expires_at,
            },
        );
        Ok(ProviderAuthStart {
            operation_id,
            status: "pending".to_string(),
            verification_uri: url.to_string(),
            expires_at,
            retry_after_ms: 1_000,
        })
    }

    pub async fn poll(&self, operation_id: &str) -> ProviderAuthPoll {
        let (outcome, expires_at) = {
            let operations = self.operations.lock().await;
            let Some(op) = operations.get(operation_id) else {
                return ProviderAuthPoll::OperationLost;
            };
            (op.outcome.clone(), op.expires_at)
        };
        let finished = outcome.lock().await.clone();
        match finished {
            Some(Outcome::Completed(material)) => {
                self.operations.lock().await.remove(operation_id);
                ProviderAuthPoll::Completed(material)
            }
            Some(Outcome::Failed(failure)) => {
                self.operations.lock().await.remove(operation_id);
                ProviderAuthPoll::Failed(failure)
            }
            None if oauth::now_secs() > expires_at + 5 => {
                self.cancel(operation_id).await;
                ProviderAuthPoll::OperationLost
            }
            None => ProviderAuthPoll::Pending {
                retry_after_ms: Some(1_000),
            },
        }
    }

    pub async fn cancel(&self, operation_id: &str) {
        if let Some(op) = self.operations.lock().await.remove(operation_id) {
            op.cancel.cancel();
        }
    }
}

async fn bind_loopback() -> Result<TcpListener, ProviderRpcError> {
    match TcpListener::bind(("127.0.0.1", oauth::PREFERRED_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| ProviderRpcError::Unavailable("login callback unavailable".into())),
    }
}

struct Flow {
    endpoints: Endpoints,
    state: StateFile,
    http: reqwest::Client,
    attempt: Attempt,
    saved: Option<Registration>,
}

impl Flow {
    async fn run(self, listener: TcpListener) -> Result<CredentialMaterial, ProviderRpcFailure> {
        let (mut socket, callback) = wait_for_callback(&listener).await?;
        let result = self.finish(&callback).await;
        let page = match &result {
            Ok(_) => page::signed_in(),
            Err(failure) => page::failed(&failure.message),
        };
        let _ = respond(&mut socket, 200, Body::Html(&page)).await;
        result
    }

    async fn finish(
        &self,
        callback: &oauth::Callback,
    ) -> Result<CredentialMaterial, ProviderRpcFailure> {
        let (code, client_id) = oauth::accept_callback(&self.attempt, callback)?;
        let registering = self.saved.is_none();
        if registering {
            self.state
                .set_registration(Registration {
                    client_id: client_id.clone(),
                    subject: None,
                    email: None,
                })
                .map_err(|_| {
                    failure(
                        "state_unwritable",
                        "Could not save the ChatGPT registration.",
                    )
                })?;
        }
        let tokens = oauth::exchange_code(
            &self.http,
            &self.endpoints,
            &self.attempt,
            &code,
            &client_id,
        )
        .await
        .map_err(|error| match error {
            TokenError::Rejected(code) if code == "invalid_grant" => failure(
                "invalid_grant",
                "The sign-in code expired before Gray could use it; run /connect again.",
            ),
            TokenError::Rejected(code) => failure(&code, "ChatGPT rejected the sign-in."),
            TokenError::Unavailable => ProviderRpcFailure {
                code: "unavailable".to_string(),
                message: "ChatGPT sign-in service unavailable; try again.".to_string(),
                retryable: true,
                terminal: false,
            },
        })?;
        let id_token = tokens
            .id_token
            .as_deref()
            .ok_or_else(|| failure("invalid_id_token", "ChatGPT returned no ID token."))?;
        let identity = id_token::verify(
            &self.http,
            &self.endpoints,
            id_token,
            &client_id,
            &self.attempt.nonce,
        )
        .await?;

        if !tokens.scopes().contains(&oauth::PLAN_USAGE_SCOPE) {
            return Err(failure(
                "plan_usage_not_granted",
                "Signed in, but permission to use your ChatGPT plan was not granted. \
                 Run /connect again and allow plan usage.",
            ));
        }
        self.state
            .set_registration(Registration {
                client_id: client_id.clone(),
                subject: Some(identity.subject.clone()),
                email: identity.email.clone(),
            })
            .map_err(|_| {
                failure(
                    "state_unwritable",
                    "Could not save the ChatGPT registration.",
                )
            })?;
        oauth::credential(
            &tokens,
            &client_id,
            &identity.subject,
            identity.email.as_deref(),
            None,
            None,
        )
    }
}

async fn wait_for_callback(
    listener: &TcpListener,
) -> Result<(TcpStream, oauth::Callback), ProviderRpcFailure> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|_| failure("unavailable", "Sign-in callback failed."))?;
        let Some(target) = read_target(&mut socket).await else {
            continue;
        };
        match oauth::parse_callback(&target) {
            Some(callback) => return Ok((socket, callback)),
            None => {
                let _ = respond(&mut socket, 404, Body::Text("Not found")).await;
            }
        }
    }
}

async fn read_target(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(10), socket.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
        if read == 0 || head.len() + read > MAX_HEAD_BYTES {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next()?.split_whitespace();
    if request_line.next()? != "GET" {
        return None;
    }
    Some(request_line.next()?.to_string())
}

enum Body<'a> {
    Html(&'a str),
    Text(&'a str),
}

async fn respond(socket: &mut TcpStream, status: u16, body: Body<'_>) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let (content_type, extra, body) = match body {
        Body::Html(body) => (
            "text/html",
            format!(
                "Content-Security-Policy: {}\r\nReferrer-Policy: no-referrer\r\n",
                page::CSP
            ),
            body,
        ),
        Body::Text(body) => ("text/plain", String::new(), body),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         {extra}Connection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

pub async fn interactive_login(
    endpoints: &Endpoints,
    state: &StateFile,
    http: &reqwest::Client,
    label: &str,
) -> Result<CredentialMaterial, String> {
    let host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4().hyphenated());
    let listener = bind_loopback().await.map_err(|e| e.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("login callback unavailable: {e}"))?
        .port();
    let attempt = Attempt::new(None, port);
    let url = oauth::authorization_url(endpoints, &attempt, &host_id, None);
    println!("Sign in with ChatGPT for '{label}':\n  {url}\n\nWaiting for browser sign-in...");
    open_browser(url.as_str());
    let flow = Flow {
        endpoints: endpoints.clone(),
        state: state.clone(),
        http: http.clone(),
        attempt,
        saved: None,
    };
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(oauth::LOGIN_TTL_SECS));
    tokio::select! {
        _ = deadline => Err("Sign-in timed out.".to_string()),
        res = flow.run(listener) => res.map_err(|f| f.message),
    }
}

fn open_browser(url: &str) {
    let prog = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(prog)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
