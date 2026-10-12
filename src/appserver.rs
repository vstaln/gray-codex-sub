//! One-shot `codex app-server` JSON-RPC probes: spawn → initialize →
//! call(s) → kill. Used by status, usage, models, and the auth probe;
//! the chat path keeps its own pooled children in `session.rs`.
//!
//! The wire is NDJSON on stdio: client→server `{method, id, params}`
//! requests plus `initialized` notification; server→client responses
//! `{id, result|error}`, notifications (`method`, no id), and its own
//! requests (`method` + `id`). A probe has no tools or approvals to
//! offer, so every server→client request is refused (-32601).
use crate::setup;
use gray_plugin::ProviderRpcError;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
/// No single call may stall a probe forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
#[derive(Debug)]
pub enum Error {
    /// `codex` is not on PATH and no override is set.
    NotInstalled,
    Io(String),
    Timeout,
    /// The server answered the request with an `error` object.
    Rpc(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotInstalled => write!(f, "codex not installed"),
            Error::Io(m) => write!(f, "{m}"),
            Error::Timeout => write!(f, "app-server call timed out"),
            Error::Rpc(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for Error {}
/// Map a probe failure to the wire error the host displays. Auth-shaped
/// failures get the login hint; everything else reports its own text.
impl From<Error> for ProviderRpcError {
    fn from(e: Error) -> ProviderRpcError {
        match e {
            Error::NotInstalled => ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()),
            other => {
                let m = other.to_string();
                if is_auth_error(&m) {
                    ProviderRpcError::Unavailable(setup::LOGIN_HINT.into())
                } else {
                    ProviderRpcError::Unavailable(m)
                }
            }
        }
    }
}
enum Wire {
    Msg(Value),
    Eof,
}
/// A live probe child. Killed on drop — `Child`'s own Drop does not.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    _stage: tempfile::TempDir,
    rx: mpsc::Receiver<Wire>,
    next_id: u64,
}
impl Client {
    /// Spawn `codex app-server`, run the initialize handshake, and return
    /// a client ready for calls.
    pub fn spawn() -> Result<Client, Error> {
        let bin = setup::resolve_command().ok_or(Error::NotInstalled)?;
        let home = setup::codex_home();
        std::fs::create_dir_all(&home).map_err(|e| Error::Io(format!("codex home: {e}")))?;
        let stage = tempfile::Builder::new()
            .prefix("codex-sub-probe-")
            .tempdir()
            .map_err(|e| Error::Io(format!("stage dir: {e}")))?;
        let mut cmd = Command::new(&bin);
        cmd.arg("app-server")
            .current_dir(stage.path())
            .envs(setup::child_env(&home))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Io(format!("spawn {bin}: {e}")))?;
        let kill = |child: &mut Child| {
            let _ = child.kill();
            let _ = child.wait();
        };
        let (stdout, stdin) = match (child.stdout.take(), child.stdin.take()) {
            (Some(o), Some(i)) => (o, i),
            _ => {
                kill(&mut child);
                return Err(Error::Io("spawn: missing stdio pipe".to_string()));
            }
        };
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("codex-sub-probe".into())
            .spawn(move || read_lines(stdout, tx));
        if let Err(e) = reader {
            kill(&mut child);
            return Err(Error::Io(format!("reader thread: {e}")));
        }
        let mut client = Client {
            child,
            stdin,
            _stage: stage,
            rx,
            next_id: 1,
        };
        client.call(
            "initialize",
            json!({
                "clientInfo": {"name": "gray-codex-sub", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true},
            }),
        )?;
        client.send(&json!({"method": "initialized", "params": {}}))?;
        Ok(client)
    }
    fn send(&mut self, msg: &Value) -> Result<(), Error> {
        let body = serde_json::to_string(msg).map_err(|e| Error::Io(format!("encode: {e}")))?;
        self.stdin
            .write_all(body.as_bytes())
            .and_then(|()| self.stdin.write_all(b"\n"))
            .and_then(|()| self.stdin.flush())
            .map_err(|e| Error::Io(format!("write: {e}")))
    }
    /// One request → one result. Notifications are skipped; server
    /// requests are refused so a probe never approves or runs anything.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"method": method, "id": id, "params": params}))?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let msg = match self.rx.recv_timeout(left) {
                Ok(Wire::Msg(v)) => v,
                Ok(Wire::Eof) => return Err(Error::Io("app-server exited".to_string())),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(Error::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::Io("reader thread died".to_string()));
                }
            };
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let rid = msg.get("id").cloned().unwrap_or(Value::Null);
                let m = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let _ = self.send(&json!({"id": rid,
                    "error": {"code": -32601,
                        "message": format!("{m} unsupported by gray-codex-sub")}}));
                continue;
            }
            if msg.get("method").is_some() {
                continue;
            }
            if msg.get("id") == Some(&Value::from(id)) {
                if let Some(err) = msg.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed");
                    return Err(Error::Rpc(format!(
                        "{method}: {}",
                        m.chars().take(200).collect::<String>()
                    )));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn read_lines(stdout: ChildStdout, tx: mpsc::Sender<Wire>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Wire::Eof);
                return;
            }
            Ok(_) => match serde_json::from_str::<Value>(line.trim()) {
                Ok(v) => {
                    if tx.send(Wire::Msg(v)).is_err() {
                        return;
                    }
                }
                Err(_) => continue,
            },
        }
    }
}
/// Run `f` against a fresh probe client; the child dies when it returns.
pub fn with_client<T>(f: impl FnOnce(&mut Client) -> Result<T, Error>) -> Result<T, Error> {
    let mut client = Client::spawn()?;
    f(&mut client)
}
/// What `account/read` found.
#[derive(Debug)]
pub enum Probe {
    /// `account` non-null. Carries the full GetAccountResponse.
    Ready(Value),
    /// `account` null: no usable login for this Codex home.
    NotSignedIn(Value),
    NotInstalled,
    Failed(String),
}
/// Who is signed in, per the CLI's own stored login.
pub fn probe() -> Probe {
    match with_client(|c| c.call("account/read", json!({}))) {
        Err(Error::NotInstalled) => Probe::NotInstalled,
        Err(e) => Probe::Failed(e.to_string()),
        Ok(v) => {
            if v.get("account").is_some_and(|a| !a.is_null()) {
                Probe::Ready(v)
            } else {
                Probe::NotSignedIn(v)
            }
        }
    }
}
/// An error string that smells like "not signed in" — mapped to the
/// login hint rather than surfaced raw.
pub fn is_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("unauthorized")
        || m.contains("not logged in")
        || m.contains("not signed in")
        || m.contains("authentication required")
        || m.contains("401")
}
