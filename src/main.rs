//! codex-sub: a protocol-1.2 sidecar for Gray.
//!
//! Codex-backend route through the official Codex CLI: sign-in is the
//! CLI's own job (`codex login` against the plugin's CODEX_HOME), and
//! chat runs through pooled `codex app-server` children (one Codex
//! thread per conversation) behind a per-turn loopback relay. The
//! sidecar holds no tokens — the app-server authenticates with the
//! CLI's stored login.
//!
//! Startup is deliberately unprivileged: the `manifest` argv shortcut and
//! the `plugin/manifest` handshake must survive hosts where the tokio
//! runtime, spare threads, or a writable HOME are missing. Everything
//! optional degrades to a per-method structured error — nothing before
//! the read loop may panic or exit.
//!
//! `command/run`: a bare `/codex` answers nothing so the host's
//! provider-login shortcut switches the session to Codex; `/codex tools …`
//! owns the upstream tool allowlist (`{"text": …}`).

use codex_sub::{appserver, chat, manifest, models, relay, session, setup, usage};

use std::io::{BufRead, Write};
use std::sync::Arc;

use gray_core::credential::CredentialMaterial;
use gray_plugin::{
    ProviderAuthPoll, ProviderAuthStart, ProviderModelsRequest, ProviderRefreshRequest,
    ProviderRevokeRequest, ProviderRpcError, ProviderRpcFailure,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Diagnostics live on stderr only — stdout carries protocol frames.
fn diag(message: &str) {
    eprintln!("codex-sub: {message}");
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
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

    // `gray codex-sub tools …` and the REPL's `/codex tools …` land on
    // the same file: one allowlist, no sidecar needed.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("tools")) {
        let args: Vec<String> = std::env::args_os()
            .skip(2)
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        println!("{}", codex_sub::settings::tools_command(&args));
        return;
    }

    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("status")) {
        println!("{}", codex_sub::status::text());
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
        relays: Default::default(),
    };
    if let Err(e) = session::start_reaper() {
        diag(&format!(
            "idle-session reaper unavailable ({e}); pooled sessions will not be reaped"
        ));
    }
    // Re-pool sessions persisted before a restart so their upstream
    // cache entries renew instead of lapsing — best-effort, background.
    let _ = std::thread::Builder::new()
        .name("codex-sub-rewarm".into())
        .spawn(session::rewarm);
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
        if request.id.is_null() {
            if request.method == "plugin/shutdown" {
                session::shutdown_all();
                return;
            }
            continue;
        }
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
        if request.id.is_null() {
            if request.method == "plugin/shutdown" {
                session::shutdown_all();
                return;
            }
            continue;
        }
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
        relays: Default::default(),
    });
    if let Err(e) = session::start_reaper() {
        diag(&format!(
            "idle-session reaper unavailable ({e}); pooled sessions will not be reaped"
        ));
    }
    // Re-pool sessions persisted before a restart so their upstream
    // cache entries renew instead of lapsing — best-effort, background.
    let _ = std::thread::Builder::new()
        .name("codex-sub-rewarm".into())
        .spawn(session::rewarm);
    let mut stdout = std::io::stdout();
    while let Some(line) = stdin_rx.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        if request.id.is_null() {
            if request.method == "plugin/shutdown" {
                session::shutdown_all();
                return;
            }
            continue;
        }
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
            match blocking(appserver::probe).await? {
                appserver::Probe::Ready(_) => Ok(serde_json::to_value(ProviderAuthStart {
                    operation_id: String::new(),
                    status: "completed".to_string(),
                    verification_uri: String::new(),
                    expires_at: now_secs() + 600,
                    retry_after_ms: 0,
                })
                .unwrap()),
                appserver::Probe::NotInstalled => {
                    Err(ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()))
                }
                appserver::Probe::NotSignedIn(_) => {
                    Err(ProviderRpcError::Unavailable(setup::LOGIN_HINT.into()))
                }
                appserver::Probe::Failed(e) => Err(ProviderRpcError::Unavailable(e)),
            }
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
            // The probe is synchronous — there is no pending operation.
            let poll = match blocking(appserver::probe).await? {
                appserver::Probe::Ready(account) => {
                    ProviderAuthPoll::Completed(login_material(&account))
                }
                appserver::Probe::NotSignedIn(_) => ProviderAuthPoll::Failed(ProviderRpcFailure {
                    code: "unavailable".to_string(),
                    message: setup::LOGIN_HINT.into(),
                    retryable: false,
                    terminal: true,
                }),
                appserver::Probe::NotInstalled => ProviderAuthPoll::Failed(ProviderRpcFailure {
                    code: "unavailable".to_string(),
                    message: setup::INSTALL_HINT.into(),
                    retryable: false,
                    terminal: true,
                }),
                appserver::Probe::Failed(e) => ProviderAuthPoll::Failed(ProviderRpcFailure {
                    code: "unavailable".to_string(),
                    message: e,
                    retryable: true,
                    terminal: false,
                }),
            };
            Ok(poll_value(poll))
        }
        "provider/auth/cancel" => Ok(json!({})),
        "provider/auth/refresh" => {
            let request: ProviderRefreshRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider refresh request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            // "Refresh" re-probes the CLI login: tokens live in the
            // Codex home, so a fresh marker is all the host needs.
            match blocking(appserver::probe).await? {
                appserver::Probe::Ready(account) => {
                    Ok(serde_json::to_value(login_material(&account)).unwrap())
                }
                appserver::Probe::NotSignedIn(_) => {
                    Err(ProviderRpcError::Rpc(ProviderRpcFailure {
                        code: "invalid_grant".to_string(),
                        message: setup::LOGIN_HINT.into(),
                        retryable: false,
                        terminal: true,
                    }))
                }
                appserver::Probe::NotInstalled => {
                    Err(ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()))
                }
                appserver::Probe::Failed(e) => Err(ProviderRpcError::Unavailable(e)),
            }
        }
        "provider/auth/revoke" => {
            let request: ProviderRevokeRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider revoke request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            // Revoke clears only what this plugin ever wrote: the
            // retired token mirror. The real login belongs to the CLI —
            // `codex logout` removes it.
            let _ = std::fs::remove_file(setup::legacy_auth_file());
            Ok(json!({"status": "unsupported", "message": REVOKE_MSG}))
        }
        "provider/usage" => blocking(usage::handle).await?,
        "provider/models" => {
            let request: ProviderModelsRequest =
                serde_json::from_value(params).map_err(|_| invalid("provider models request"))?;
            ensure_provider(&request.provider, &request.auth_method)?;
            let models = blocking(models::fetch).await??;
            Ok(serde_json::to_value(models).unwrap())
        }
        "provider/chat" => chat_turn(&shared.relays, params).await,
        "command/run" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if argv.first().map(String::as_str) == Some("status") {
                return Ok(json!({"text": blocking(codex_sub::status::text).await?}));
            }
            manifest::run_command(name, &argv)
                .ok_or_else(|| ProviderRpcError::Protocol("unknown command".to_string()))
        }
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
    if !setup::cli_login_present() {
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

/// Codex CLI credentials belong to the Codex CLI; nothing here can
/// revoke them upstream.
const REVOKE_MSG: &str = "Codex credentials live in the Codex CLI home (CODEX_HOME). Sign out with \
    `codex logout`. This plugin stores no credentials to revoke.";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
/// Run a blocking probe off the async workers — the probe itself spawns
/// a short-lived `codex app-server` child.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ProviderRpcError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ProviderRpcError::Unavailable(format!("probe task failed: {e}")))
}
/// The marker handed to the host on a completed sign-in: the real login
/// is the Codex CLI's own `auth.json` under CODEX_HOME — never proxied
/// and never read by this plugin. The marker only records that a
/// signed-in CLI was observed.
fn login_material(account: &Value) -> CredentialMaterial {
    let mut material = CredentialMaterial::default();
    material
        .secrets
        .insert("codex_cli_login".to_string(), "1".to_string());
    if let Some(v) = setup::cli_version() {
        material.metadata.insert("cli_version".to_string(), v);
    }
    let acct = account.get("account").cloned().unwrap_or(Value::Null);
    if let Some(email) = acct.get("email").and_then(Value::as_str) {
        material
            .metadata
            .insert("account".to_string(), email.to_string());
    }
    if let Some(plan) = acct.get("planType").and_then(Value::as_str) {
        material
            .metadata
            .insert("plan".to_string(), plan.to_string());
    }
    material.expires_at = None;
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
