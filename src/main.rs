//! codex-sub: a protocol-1.2 sidecar for Gray.
//!
//! Unofficial Codex-backend route: logs in with the Codex CLI's own OAuth
//! client, then serves chat through pooled `codex app-server` children
//! (one Codex thread per conversation) behind a per-turn loopback relay.
//!
//! Startup is deliberately unprivileged: the `manifest` argv shortcut and
//! the `plugin/manifest` handshake must survive hosts where the tokio
//! runtime, spare threads, or a writable HOME are missing. Everything
//! optional degrades to a per-method structured error — nothing before
//! the read loop may panic or exit.

use codex_sub::{chat, login, manifest, models, oauth, relay, session, setup};

use std::io::{BufRead, Write};
use std::sync::{Arc, OnceLock};

use gray_plugin::{
    ProviderAuthPoll, ProviderModelsRequest, ProviderRefreshRequest, ProviderRevokeRequest,
    ProviderRpcError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::login::LoginManager;
use crate::oauth::OAuthConfig;

/// Diagnostics live on stderr only — stdout carries protocol frames.
fn diag(message: &str) {
    eprintln!("codex-sub: {message}");
}

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

/// State the request handlers share; built inside `serve` so its pieces
/// stay plain values (nothing here touches the OS).
struct Shared {
    logins: LoginManager,
    config: OAuthConfig,
    relays: relay::Intents,
}

fn main() {
    // `gray plugin add` and registration probe `<bin> manifest`. This path
    // builds no runtime, spawns no threads, and touches no directories —
    // a host under resource limits still has to see the manifest.
    // (args_os: a non-UTF8 argv must not panic us.)
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("manifest")) {
        // Serialize through to_value exactly like the wire `result` field
        // so `codex-sub manifest` is byte-identical to `plugin/manifest`.
        let out = serde_json::to_value(manifest::manifest())
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "{}".to_string());
        let mut stdout = std::io::stdout().lock();
        if writeln!(stdout, "{out}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            diag("manifest write failed: stdout closed");
            std::process::exit(2);
        }
        return;
    }

    let Some(runtime) = build_runtime() else {
        diag("no tokio runtime available; degraded mode (manifest + errors only)");
        serve_degraded();
        return;
    };

    // stdin rides a plain std thread feeding a channel — not
    // `tokio::io::stdin` (the blocking pool), because a thread-starved
    // host is exactly the case this ladder exists for. If even one
    // thread cannot spawn, the runtime still drives each request inline
    // (`serve_serial`) — manifest, auth, refresh and models all keep
    // working; only paths that need their own threads fail closed.
    // (Spawned only once a runtime exists: a pump with no consumer
    // would hold the stdin lock and starve the degraded loop.)
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let stdin_live = std::thread::Builder::new()
        .name("codex-sub-stdin".into())
        .spawn(move || stdin_pump(stdin_tx))
        .is_ok();
    if !stdin_live {
        diag("cannot spawn the stdin reader thread; serving serially on the runtime");
        serve_serial(&runtime);
        return;
    }

    runtime.block_on(serve(stdin_rx));
}

/// Copy stdin lines into the async loop. EOF or a read error drops the
/// sender, which `serve` sees as a clean shutdown.
fn stdin_pump(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        match lines.next() {
            Some(Ok(line)) => {
                if tx.send(line).is_err() {
                    return;
                }
            }
            Some(Err(e)) => {
                diag(&format!("stdin read failed: {e}"));
                return;
            }
            None => return,
        }
    }
}

/// Build the least-bad runtime that works on this host. Each step logs
/// and falls through rather than dying: a sidecar that can still answer
/// `plugin/manifest` beats a silent exit.
///
/// `build` must be caught, not matched: a starved host makes tokio's
/// multi-thread builder *panic* ("OS can't spawn worker thread") — the
/// exact spawn-time kill this ladder exists to survive.
fn build_runtime() -> Option<tokio::runtime::Runtime> {
    // Two workers is plenty — requests are sequential and the heavy work
    // lives on session/relay threads, not on the scheduler. Capping the
    // pool also keeps `build` alive near a pids/thread limit (the default
    // spawns one worker per CPU).
    match std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
    }) {
        Ok(Ok(runtime)) => return Some(runtime),
        Ok(Err(e)) => diag(&format!("multi-thread tokio runtime unavailable: {e}")),
        Err(_) => diag("multi-thread tokio runtime panicked during init (thread-starved host?)"),
    }
    match std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    }) {
        Ok(Ok(runtime)) => {
            diag("running on a current-thread tokio runtime");
            return Some(runtime);
        }
        Ok(Err(e)) => diag(&format!("tokio runtime (io/time drivers) unavailable: {e}")),
        Err(_) => diag("tokio runtime panicked during init"),
    }
    // Bare scheduler: the wire loop still runs; methods needing sockets
    // or timers report `unavailable` per call instead of taking the
    // sidecar down.
    match std::panic::catch_unwind(|| tokio::runtime::Builder::new_current_thread().build()) {
        Ok(Ok(runtime)) => {
            diag(
                "running without tokio io/time drivers; login/network methods will report unavailable",
            );
            Some(runtime)
        }
        Ok(Err(e)) => {
            diag(&format!("tokio scheduler unavailable: {e}"));
            None
        }
        Err(_) => {
            diag("tokio scheduler panicked during init");
            None
        }
    }
}

/// A runtime exists but no spare thread for the stdin pump: read stdin
/// on the main thread and drive each request through `block_on`. Almost
/// everything keeps working — a login's callback task progresses during
/// subsequent requests, and only the paths that spawn their own threads
/// (relay, session children) fail closed per call.
fn serve_serial(runtime: &tokio::runtime::Runtime) {
    let shared = Shared {
        logins: LoginManager::default(),
        config: OAuthConfig::default(),
        relays: Default::default(),
    };
    session::install_oauth(shared.config.clone());
    if let Err(e) = session::start_reaper() {
        diag(&format!(
            "idle-session reaper unavailable ({e}); pooled sessions will not be reaped"
        ));
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                diag(&format!("stdin read failed: {e}"));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let method = request.method.clone();
        // A handler panic unwinds block_on — catch it into one error
        // frame instead of taking the sidecar down.
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(handle(&shared, &request))
        })) {
            Ok(outcome) => outcome,
            Err(_) => {
                diag(&format!("{method}: handler panicked"));
                Err(ProviderRpcError::Unavailable(format!(
                    "codex-sub internal failure handling {method}"
                )))
            }
        };
        if !write_response(&mut stdout, request.id, outcome) {
            return;
        }
        if method == "plugin/shutdown" {
            session::shutdown_all();
            return;
        }
    }
}

/// No runtime at all: a pure-std loop that still speaks the wire —
/// manifest and shutdown answer, everything else gets a structured
/// `unavailable` so the host sees a live sidecar.
fn serve_degraded() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                diag(&format!("stdin read failed: {e}"));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let shutdown = request.method == "plugin/shutdown";
        let outcome = match request.method.as_str() {
            "plugin/manifest" => {
                Ok(serde_json::to_value(manifest::manifest()).unwrap_or(Value::Null))
            }
            "plugin/shutdown" => Ok(json!({})),
            _ => Err(ProviderRpcError::Unavailable(
                "codex-sub is running degraded (no runtime/threads on this host); \
                 provider methods unavailable"
                    .to_string(),
            )),
        };
        if !write_response(&mut stdout, request.id, outcome) {
            return;
        }
        if shutdown {
            return;
        }
    }
}

/// Serialize + write one frame. Returns false when stdout is gone —
/// the host abandoned us, so exit quietly.
fn write_response(
    stdout: &mut impl Write,
    id: Value,
    outcome: Result<Value, ProviderRpcError>,
) -> bool {
    let response = match outcome {
        Ok(result) => Response {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id,
            result: None,
            error: Some(error_value(error)),
        },
    };
    let frame = serde_json::to_string(&response)
        .unwrap_or_else(|_| "{\"id\":null,\"error\":{\"code\":\"internal\"}}".to_string());
    // Protocol frames only: never log credential payloads or upstream bodies.
    if writeln!(stdout, "{frame}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        diag("stdout write failed; host is gone, exiting");
        return false;
    }
    true
}

async fn serve(mut stdin_rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
    let shared = Arc::new(Shared {
        logins: LoginManager::default(),
        config: OAuthConfig::default(),
        relays: Default::default(),
    });
    session::install_oauth(shared.config.clone());
    if let Err(e) = session::start_reaper() {
        diag(&format!(
            "idle-session reaper unavailable ({e}); pooled sessions will not be reaped"
        ));
    }
    let mut stdout = std::io::stdout();
    while let Some(line) = stdin_rx.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        // Each request runs as its own task so a handler panic degrades
        // to one error frame instead of taking the whole sidecar down.
        let id = request.id.clone();
        let method = request.method.clone();
        let shared2 = shared.clone();
        let outcome = match tokio::spawn(async move { handle(&shared2, &request).await }).await {
            Ok(outcome) => outcome,
            Err(join) => {
                diag(&format!("{method}: handler task failed: {join}"));
                Err(ProviderRpcError::Unavailable(format!(
                    "codex-sub internal failure handling {method}"
                )))
            }
        };
        if !write_response(&mut stdout, id, outcome) {
            break;
        }
        if method == "plugin/shutdown" {
            session::shutdown_all();
            return;
        }
    }
}

async fn handle(shared: &Shared, request: &Request) -> Result<Value, ProviderRpcError> {
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
            let start = shared.logins.start(&shared.config).await?;
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
            let poll = shared.logins.poll(operation_id).await;
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
            shared.logins.cancel(operation_id).await;
            Ok(json!({}))
        }
        "provider/auth/refresh" => {
            let request: ProviderRefreshRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider refresh request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            // Mirror the sighting first: whichever way this grant lands,
            // the cache learns what the host last handed us.
            setup::observe(&request.credential.credential);
            match oauth::refresh_material(&shared.config, &request.credential.credential).await {
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
                            let tokens = oauth::refresh_cached(&shared.config, &rt)
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
            let models = models::fetch_models(http()?, access, account_id).await?;
            // Model fetches carry the credential too: keep the mirror warm.
            setup::observe(&request.credential.credential);
            Ok(serde_json::to_value(models).unwrap())
        }
        "provider/chat" => chat_turn(&shared.relays, params).await,
        "plugin/shutdown" => Ok(json!({})),
        _ => Err(ProviderRpcError::Protocol(
            "unknown provider method".to_string(),
        )),
    }
}

/// The OAuth HTTP client is built lazily on first need: a host that can
/// run threads but can't init TLS still gets the manifest and clean
/// `unavailable` answers instead of a spawn-time exit.
fn http() -> Result<&'static reqwest::Client, ProviderRpcError> {
    static HTTP: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    HTTP.get_or_init(|| match oauth::http_client() {
        Ok(client) => Some(client),
        Err(e) => {
            diag(&format!("login HTTP client unavailable: {e:#}"));
            None
        }
    })
    .as_ref()
    .ok_or_else(|| ProviderRpcError::Unavailable("provider HTTP client unavailable".to_string()))
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
