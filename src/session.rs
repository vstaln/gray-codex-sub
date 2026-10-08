//! Persistent `codex app-server` children: one process per conversation,
//! one Codex thread inside it, reused across turns.
//!
//! Wire: JSON-RPC over NDJSON stdio. Client→server: `initialize`,
//! `initialized`, `account/login/start` (`chatgptAuthTokens`),
//! `thread/start`, `thread/inject_items`, `turn/start`, `turn/interrupt`.
//! Server→client: notifications (`item/*`, `turn/*`, `thread/*`) plus
//! *requests* — `item/tool/call` (parked until the host answers through
//! the next relay turn) and `account/chatgptAuthTokens/refresh` (answered
//! from the credential cache, refreshing via OAuth first when stale).
//!
//! Pooling mirrors the Devin sidecar: strict continuation matching (an
//! incoming history must equal what this session last absorbed plus the
//! echo of what it last emitted plus a new tail), bounded admission,
//! keepalive turns on idle sessions, and an idle reaper. A suspended
//! session — mid-turn, parked tool calls awaiting the host — stays
//! pooled and is claimed only by a request whose tail answers them.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::chat::{self, OutItem, PreparedTurn, TurnResult, Usage};
use crate::oauth::{self, OAuthConfig};
use crate::setup;

/// Pooled children per sidecar process.
const MAX_SESSIONS: usize = 4;
/// Handshake + request budget for `initialize`/login/thread-start.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A parked `item/tool/call` freezes its Codex turn: after this much
/// silence the gray turn ends with the emitted calls and the session is
/// suspended in the pool.
const PARK_QUIESCE: Duration = Duration::from_secs(10);
/// Pooled session is killed after this much real-prompt idleness.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// An idle session gets one cache-warming keepalive turn after this.
const KEEPALIVE_AFTER: Duration = Duration::from_secs(10 * 60);
/// Reaper sweep cadence.
const REAPER_TICK: Duration = Duration::from_secs(30);
/// How long an interrupted turn has to settle before the child is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// stderr is captured for diagnostics only; never logged (it can carry
/// upstream detail we don't want in trace files).
const STDERR_RING: usize = 40;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The OAuth config is installed once at startup; session threads answer
/// app-server token refreshes without going back through the host.
static OAUTH: OnceLock<OAuthConfig> = OnceLock::new();

pub fn install_oauth(config: OAuthConfig) {
    let _ = OAUTH.set(config);
}

enum Wire {
    Msg(Value),
    /// Unparseable NDJSON line — counted, tolerated.
    Bad,
    /// stdout EOF: the child is gone.
    Eof,
}

/// A server request we are deliberately not answering yet: the host will
/// resolve it through a `function_call_output` next turn.
struct ParkedCall {
    request_id: Value,
    call_id: String,
}

/// Per-item accumulation while a turn streams.
#[derive(Default)]
struct ItemAcc {
    kind: String,
    /// Text seen through deltas (fallback when `item/completed` carries
    /// no body).
    delta: String,
}

pub struct LiveSession {
    child: Child,
    stdin: ChildStdin,
    inbox: mpsc::Receiver<Wire>,
    stderr_ring: Arc<Mutex<VecDeque<String>>>,
    _stage: tempfile::TempDir,
    next_id: u64,

    thread_id: String,
    surface: String,
    model: String,
    effort: Option<String>,

    /// The exact Responses `input` array this session last absorbed —
    /// the continuation prefix contract.
    absorbed: Vec<Value>,
    /// What the last completed turn emitted (for the echo check).
    reply_call_ids: Vec<String>,
    reply_text: String,

    /// A Codex turn is open (`turn/started` seen, no `turn/completed`).
    /// On error this decides close-vs-keep, and blocks pool reclamation
    /// of a turn that never settled.
    pub prompt_in_flight: bool,
    /// Turn open but frozen on parked tool calls; pooled awaiting answers.
    suspended: bool,
    parked: Vec<ParkedCall>,

    items: BTreeMap<String, ItemAcc>,
    out: Vec<OutItem>,
    /// Cumulative thread token usage as last reported
    /// (`thread/tokenUsage/updated` `total`).
    usage_total: Usage,
    /// `usage_total` snapshot at turn start — a turn's usage is the diff.
    usage_turn_start: Usage,
    pending_error: Option<String>,
    current_turn_id: Option<String>,

    last_used: Instant,
    last_prompt_at: Instant,
    /// The claiming turn's disconnect flag — stored so nested handling
    /// (auth refresh, interrupt) can observe it.
    cancel: Arc<AtomicBool>,
    closed: bool,
}

static POOL: Mutex<Vec<LiveSession>> = Mutex::new(Vec::new());

/// What a claimed session should do with the new request.
pub enum Claim {
    /// Idle turn boundary: send `turn/start` with these raw tail items.
    Delta { tail: Vec<Value>, undelivered: bool },
    /// Suspended turn: resolve these `function_call_output` items against
    /// parked calls, then keep driving the same Codex turn.
    Suspended { outputs: Vec<Value> },
}

impl Claim {
    pub fn kind(&self) -> &'static str {
        match self {
            Claim::Delta { .. } => "delta",
            Claim::Suspended { .. } => "resume",
        }
    }
}

/// Try to claim a pooled session for this turn. Strict match only:
/// identical surface (developer text + tools), model, effort, then the
/// continuation contract. Returns the miss reason for the trace.
pub fn take(
    turn: &PreparedTurn,
    cancel: &Arc<AtomicBool>,
) -> (Option<(LiveSession, Claim)>, &'static str) {
    let mut pool = match POOL.lock() {
        Ok(p) => p,
        Err(e) => e.into_inner(),
    };
    let mut reason = "new";
    let mut idx_found = None;
    let mut claim_found = None;
    for (i, s) in pool.iter().enumerate() {
        if s.closed {
            continue;
        }
        // A turn running free (not parked on calls) can't be claimed.
        if s.prompt_in_flight && !s.suspended {
            continue;
        }
        if s.surface != turn.surface {
            reason = "surface";
            continue;
        }
        if s.model != turn.native_model || s.effort != turn.effort {
            reason = "model";
            continue;
        }
        match chat::continuation(&s.absorbed, &s.reply_call_ids, &s.reply_text, &turn.input) {
            Ok((tail, undelivered)) => {
                if s.suspended {
                    // A frozen turn accepts only pure answers to parked calls.
                    let parked_ids: Vec<String> =
                        s.parked.iter().map(|p| p.call_id.clone()).collect();
                    if undelivered {
                        reason = "suspended-undelivered";
                        continue;
                    }
                    if !chat::outputs_for(tail, &parked_ids) {
                        reason = "suspended-tail";
                        continue;
                    }
                    idx_found = Some(i);
                    claim_found = Some(Claim::Suspended {
                        outputs: tail.to_vec(),
                    });
                    break;
                }
                idx_found = Some(i);
                claim_found = Some(Claim::Delta {
                    tail: tail.to_vec(),
                    undelivered,
                });
                break;
            }
            Err(why) => {
                reason = why;
            }
        }
    }
    match idx_found {
        Some(i) => {
            let mut s = pool.remove(i);
            s.cancel = cancel.clone();
            (Some((s, claim_found.unwrap())), reason)
        }
        None => (None, reason),
    }
}

/// Return a session to the pool (bounded; overflow closes).
pub fn give_back(s: LiveSession) {
    if s.closed {
        return;
    }
    let mut pool = match POOL.lock() {
        Ok(p) => p,
        Err(e) => e.into_inner(),
    };
    if pool.len() >= MAX_SESSIONS {
        // Evict the least-recently-used to make room.
        if let Some((i, _)) = pool
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.last_used)
            .map(|(i, s)| (i, s.last_used))
        {
            let mut old = pool.remove(i);
            old.close();
        }
    }
    pool.push(s);
}

/// Kill everything at sidecar shutdown.
pub fn shutdown_all() {
    let mut pool = match POOL.lock() {
        Ok(p) => p,
        Err(e) => e.into_inner(),
    };
    for mut s in pool.drain(..) {
        s.close();
    }
}

/// Spawn + initialize + external-auth login + `thread/start`. The
/// returned session has an open thread and no absorbed history yet —
/// `drive_fresh` injects and starts the first turn.
pub fn spawn(
    turn: &PreparedTurn,
    deadline: Instant,
    cancel: &Arc<AtomicBool>,
) -> Result<LiveSession, String> {
    let bin = setup::resolve_command().ok_or_else(|| setup::INSTALL_HINT.to_string())?;
    let home = setup::codex_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("codex home: {e}"))?;
    let mut auth = setup::read_cached().ok_or_else(|| setup::LOGIN_HINT.to_string())?;
    // A stale access token fails `account/login/start` outright — refresh
    // first when we still hold a usable refresh token. A failed refresh
    // is non-fatal here: the token may carry slack we can't see, and the
    // login call is the authority.
    if !auth.access_fresh(now_secs(), 30)
        && let Some(rt) = auth.refresh_token.clone()
        && let Some(cfg) = OAUTH.get()
        && let Ok(t) = oauth::refresh_cached_blocking(cfg, &rt)
    {
        auth = setup::CachedAuth {
            access_token: t.access_token,
            refresh_token: Some(t.refresh_token),
            account_id: Some(t.account_id),
            expires_at: Some(t.expires_at),
            plan_type: t.plan_type,
        };
        setup::store(&auth);
    }
    let stage = tempfile::Builder::new()
        .prefix("codex-sub-")
        .tempdir()
        .map_err(|e| format!("stage dir: {e}"))?;

    let mut cmd = Command::new(&bin);
    cmd.arg("app-server")
        .current_dir(stage.path())
        .envs(setup::child_env(&home))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn {bin}: {e}"))?;
    // Every early return past this point must reap the child — `Child`'s
    // Drop does not kill it.
    let kill = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill(&mut child);
            return Err("spawn: no stdout pipe".to_string());
        }
    };
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            kill(&mut child);
            return Err("spawn: no stdin pipe".to_string());
        }
    };
    let stderr_ring = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(err) = child.stderr.take() {
        let ring = stderr_ring.clone();
        // Best-effort: diagnostics only. A failed thread spawn just means
        // no stderr tail in later error strings.
        let _ = std::thread::Builder::new()
            .name("codex-sub-child-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(err);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Ok(mut r) = ring.lock() {
                        if r.len() >= STDERR_RING {
                            r.pop_front();
                        }
                        r.push_back(line);
                    }
                }
            });
    }
    let (tx, rx) = mpsc::channel::<Wire>();
    let reader = std::thread::Builder::new()
        .name("codex-sub-child-stdout".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Wire::Eof);
                        return;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg = match serde_json::from_str::<Value>(trimmed) {
                            Ok(v) => Wire::Msg(v),
                            Err(_) => Wire::Bad,
                        };
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    if let Err(e) = reader {
        kill(&mut child);
        return Err(format!("app-server stdout reader thread: {e}"));
    }

    let mut s = LiveSession {
        child,
        stdin,
        inbox: rx,
        stderr_ring,
        _stage: stage,
        next_id: 1,
        thread_id: String::new(),
        surface: turn.surface.clone(),
        model: turn.native_model.clone(),
        effort: turn.effort.clone(),
        absorbed: Vec::new(),
        reply_call_ids: Vec::new(),
        reply_text: String::new(),
        prompt_in_flight: false,
        suspended: false,
        parked: Vec::new(),
        items: BTreeMap::new(),
        out: Vec::new(),
        usage_total: Usage::default(),
        usage_turn_start: Usage::default(),
        pending_error: None,
        current_turn_id: None,
        last_used: Instant::now(),
        last_prompt_at: Instant::now(),
        cancel: cancel.clone(),
        closed: false,
    };

    s.handshake(turn, deadline, &auth)?;
    Ok(s)
}

/// Best-effort stderr tail for error reports — redacted to first chars.
fn stderr_tail(ring: &Arc<Mutex<VecDeque<String>>>) -> String {
    let r = match ring.lock() {
        Ok(r) => r,
        Err(e) => e.into_inner(),
    };
    r.iter()
        .rev()
        .take(3)
        .rev()
        .map(|l| l.chars().take(160).collect::<String>())
        .collect::<Vec<_>>()
        .join(" | ")
}

impl LiveSession {
    pub fn debug_label(&self) -> String {
        format!("pid={} thread={}", self.child.id(), self.thread_id)
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("app-server write: {e}"))
    }

    /// Next message before `deadline`. Server requests and notifications
    /// are NOT dispatched here — callers route them (`call`/`pump`/`turn_step`).
    fn recv(&mut self, deadline: Instant) -> Result<Value, String> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err("app-server turn deadline exceeded".into());
            }
            match self.inbox.recv_timeout(deadline - now) {
                Ok(Wire::Msg(v)) => return Ok(v),
                Ok(Wire::Bad) => continue,
                Ok(Wire::Eof) => {
                    let tail = stderr_tail(&self.stderr_ring);
                    return Err(if tail.is_empty() {
                        "app-server exited".into()
                    } else {
                        format!("app-server exited: {tail}")
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("app-server turn deadline exceeded".into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("app-server reader died".into());
                }
            }
        }
    }

    /// Handle one inbound server request during any wait. `item/tool/call`
    /// is parked; auth refresh is serviced; everything else gets a
    /// JSON-RPC error so Codex never hangs on us.
    fn on_server_request(&mut self, id: Value, method: &str, params: &Value) {
        match method {
            "item/tool/call" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = params
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = match params.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "{}".to_string(),
                };
                self.out.push(OutItem::Call {
                    call_id: call_id.clone(),
                    name,
                    args,
                });
                self.parked.push(ParkedCall {
                    request_id: id,
                    call_id,
                });
            }
            "account/chatgptAuthTokens/refresh" => match self.auth_answer() {
                Some(result) => {
                    let _ = self.send(&json!({"id": id, "result": result}));
                }
                None => {
                    let _ = self.send(&json!({"id": id,
                            "error": {"code": -32000,
                                "message": "no usable credential cached; re-run /connect codex"}}));
                }
            },
            _ => {
                let _ = self.send(&json!({
                    "id": id,
                    "error": {"code": -32601, "message": format!("{method} unsupported by gray-codex-sub")},
                }));
            }
        }
    }

    /// Answer `account/chatgptAuthTokens/refresh` from the mirrored
    /// credential, refreshing through the token endpoint when the access
    /// token is past (or near) its expiry. Never logs token material.
    fn auth_answer(&self) -> Option<Value> {
        let mut auth = setup::read_cached()?;
        if !auth.access_fresh(now_secs(), 120)
            && let Some(rt) = auth.refresh_token.clone()
            && let Some(cfg) = OAUTH.get()
            && let Ok(tokens) = oauth::refresh_cached_blocking(cfg, &rt)
        {
            auth = setup::CachedAuth {
                access_token: tokens.access_token,
                refresh_token: Some(tokens.refresh_token),
                account_id: Some(tokens.account_id),
                expires_at: Some(tokens.expires_at),
                plan_type: tokens.plan_type,
            };
            setup::store(&auth);
        }
        // `chatgptAccountId` is a required string in the response schema;
        // a cache without one can't authenticate the child.
        let account_id = auth.account_id?;
        Some(json!({
            "accessToken": auth.access_token,
            "chatgptAccountId": account_id,
            "chatgptPlanType": auth.plan_type,
        }))
    }

    fn on_notification(&mut self, method: &str, params: &Value) {
        match method {
            "turn/started" => {
                self.prompt_in_flight = true;
                self.current_turn_id = params
                    .get("turn")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "item/started" => {
                if let Some(item) = params.get("item")
                    && let Some(id) = item.get("id").and_then(Value::as_str)
                {
                    let acc = self.items.entry(id.to_string()).or_default();
                    if let Some(k) = item.get("type").and_then(Value::as_str) {
                        acc.kind = k.to_string();
                    }
                }
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                if let Some(id) = params.get("itemId").and_then(Value::as_str) {
                    let acc = self.items.entry(id.to_string()).or_default();
                    if acc.kind.is_empty() {
                        acc.kind = if method.contains("plan") {
                            "plan".into()
                        } else {
                            "agentMessage".into()
                        };
                    }
                    if let Some(d) = params.get("delta").and_then(Value::as_str) {
                        acc.delta.push_str(d);
                    }
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(id) = params.get("itemId").and_then(Value::as_str) {
                    let acc = self.items.entry(id.to_string()).or_default();
                    if acc.kind.is_empty() {
                        acc.kind = "reasoning".into();
                    }
                    if let Some(d) = params.get("delta").and_then(Value::as_str) {
                        acc.delta.push_str(d);
                    }
                }
            }
            "item/completed" => self.on_item_completed(params.get("item")),
            "thread/tokenUsage/updated" => {
                if let Some(u) =
                    Usage::from_token_usage(params.get("tokenUsage").unwrap_or(&Value::Null))
                {
                    self.usage_total = u;
                }
            }
            "error" => {
                let will_retry = params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !will_retry {
                    let msg = params
                        .get("error")
                        .and_then(|e| {
                            e.get("message")
                                .and_then(Value::as_str)
                                .or_else(|| e.as_str())
                        })
                        .unwrap_or("upstream error");
                    self.pending_error = Some(msg.chars().take(300).collect());
                }
            }
            _ => {}
        }
    }

    /// `item/completed` is authoritative for item content — prefer its
    /// payload over accumulated deltas.
    fn on_item_completed(&mut self, item: Option<&Value>) {
        let Some(item) = item else { return };
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let acc = self.items.entry(id.to_string()).or_default();
        if acc.kind.is_empty() {
            acc.kind = kind.to_string();
        }
        match kind {
            "agentMessage" => {
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| acc.delta.clone());
                self.out.push(OutItem::Message {
                    id: id.to_string(),
                    text,
                });
            }
            "reasoning" => {
                let text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| match p {
                                Value::String(s) => Some(s.clone()),
                                _ => p.get("text").and_then(Value::as_str).map(str::to_string),
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| acc.delta.clone());
                self.out.push(OutItem::Reasoning {
                    id: id.to_string(),
                    text,
                });
            }
            "plan" => {
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| acc.delta.clone());
                if !text.trim().is_empty() {
                    self.out.push(OutItem::Reasoning {
                        id: id.to_string(),
                        text: format!("[plan]\n{text}"),
                    });
                }
            }
            // Calls are emitted from the parked request, not the item.
            _ => {}
        }
    }

    /// One inbound step during a turn wait. Returns `Some(stop)` when the
    /// turn settled, `None` to keep waiting.
    fn turn_step(&mut self, msg: Value) -> Option<Result<String, String>> {
        if msg.get("method").is_some() && msg.get("id").is_some() {
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            self.on_server_request(id, &method, &params);
            return None;
        }
        if msg.get("method").is_some() {
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if method == "turn/completed" {
                self.prompt_in_flight = false;
                // Whatever remains parked dies with the turn — never let
                // stale request ids leak into the next turn's quiesce check.
                self.parked.clear();
                self.suspended = false;
                let status = params
                    .get("turn")
                    .and_then(|t| t.get("status"))
                    .map(|s| {
                        s.get("type")
                            .and_then(Value::as_str)
                            .or_else(|| s.as_str())
                            .unwrap_or("completed")
                            .to_string()
                    })
                    .unwrap_or_else(|| "completed".to_string());
                let err = params
                    .get("turn")
                    .and_then(|t| t.get("error"))
                    .and_then(|e| {
                        e.get("message")
                            .and_then(Value::as_str)
                            .or_else(|| e.as_str())
                    })
                    .map(str::to_string)
                    .or_else(|| self.pending_error.clone());
                return Some(match status.as_str() {
                    "completed" => Ok("completed".to_string()),
                    "interrupted" | "cancelled" => Ok("interrupted".to_string()),
                    _ => Err(err.unwrap_or_else(|| format!("turn {status}"))),
                });
            }
            if method == "thread/closed" {
                self.prompt_in_flight = false;
                return Some(Err("thread closed by app-server".into()));
            }
            self.on_notification(&method, &params);
            return None;
        }
        // A stray response to nothing we asked: ignore.
        None
    }

    /// Drive the open turn until it completes or suspends on parked
    /// calls. On suspend the parked calls stay unanswered — the next gray
    /// turn's outputs resolve them.
    fn pump(&mut self, deadline: Instant, cancel: &Arc<AtomicBool>) -> Result<TurnResult, String> {
        loop {
            // Parked calls freeze the upstream turn: brief silence means
            // the model is done talking and waiting on the host.
            let effective = if self.parked.is_empty() {
                deadline
            } else {
                deadline.min(Instant::now() + PARK_QUIESCE)
            };
            let msg = match self.recv(effective) {
                Ok(m) => m,
                Err(e) => {
                    if !self.parked.is_empty()
                        && Instant::now() < deadline
                        && e.contains("deadline")
                    {
                        // Quiesced with calls open: suspend into the pool.
                        self.suspended = true;
                        return Ok(self.take_result("completed", cancel));
                    }
                    if cancel.load(Ordering::Relaxed) {
                        self.interrupt();
                        match self.wait_settle(Instant::now() + INTERRUPT_GRACE) {
                            Ok(()) => return Ok(self.take_result("interrupted", cancel)),
                            Err(_) => return Err(e),
                        }
                    }
                    return Err(e);
                }
            };
            if let Some(res) = self.turn_step(msg) {
                return res.map(|stop| self.take_result(&stop, cancel));
            }
            if cancel.load(Ordering::Relaxed) && self.prompt_in_flight {
                if !self.parked.is_empty() {
                    // Client gone mid-tool-loop: fail the parked calls so
                    // the turn can die instead of freezing forever.
                    self.answer_parked_fail("(harness client disconnected)");
                }
                self.interrupt();
                let _ = self.wait_settle(Instant::now() + INTERRUPT_GRACE);
                return Ok(self.take_result("interrupted", cancel));
            }
        }
    }

    fn take_result(&mut self, stop: &str, cancel: &Arc<AtomicBool>) -> TurnResult {
        TurnResult {
            items: std::mem::take(&mut self.out),
            usage: self.usage_total.delta(&self.usage_turn_start),
            stop: stop.to_string(),
            unseen: cancel.load(Ordering::Relaxed),
        }
    }

    /// Wait for `turn/completed` after an interrupt (bounded by `grace`).
    /// True if the turn actually settled.
    fn wait_settle(&mut self, grace: Instant) -> Result<(), String> {
        loop {
            let msg = self.recv(grace)?;
            if let Some(res) = self.turn_step(msg) {
                return match res {
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                };
            }
        }
    }

    fn interrupt(&mut self) {
        if let Some(turn_id) = self.current_turn_id.clone() {
            let id = self.alloc_id();
            let _ = self.send(&json!({
                "method": "turn/interrupt",
                "id": id,
                "params": {"threadId": self.thread_id, "turnId": turn_id},
            }));
        }
    }

    /// Fail every parked call (client vanished, or keepalive noise).
    fn answer_parked_fail(&mut self, why: &str) {
        for p in std::mem::take(&mut self.parked) {
            let _ = self.send(&json!({
                "id": p.request_id,
                "result": {"success": false, "contentItems": [
                    {"type": "inputText", "text": why}
                ]},
            }));
        }
    }

    /// A `function_call_output` item → DynamicToolCallResponse result.
    fn answer_call(&mut self, request_id: Value, output: &Value) {
        let content = match output {
            Value::String(s) => vec![json!({"type": "inputText", "text": s})],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        Some(json!({"type": "inputText",
                            "text": p.get("text").and_then(Value::as_str).unwrap_or("")}))
                    }
                    Some("input_image") => {
                        let url = p.get("image_url").unwrap_or(&Value::Null);
                        let url = url
                            .as_str()
                            .or_else(|| url.get("url").and_then(Value::as_str));
                        url.filter(|u| u.starts_with("data:"))
                            .map(|u| json!({"type": "inputImage", "imageUrl": u}))
                    }
                    _ => None,
                })
                .collect(),
            other => vec![json!({"type": "inputText", "text": other.to_string()})],
        };
        let _ = self.send(&json!({
            "id": request_id,
            "result": {"success": true, "contentItems": content},
        }));
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Request/response with inline server-request dispatch: safe during
    /// handshake and turns alike.
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        let id = self.alloc_id();
        self.send(&json!({"method": method, "id": id, "params": params}))?;
        loop {
            let msg = self.recv(deadline)?;
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let m = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let rid = msg.get("id").cloned().unwrap_or(Value::Null);
                let p = msg.get("params").cloned().unwrap_or(Value::Null);
                self.on_server_request(rid, &m, &p);
                continue;
            }
            if msg.get("method").is_some() {
                let m = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let p = msg.get("params").cloned().unwrap_or(Value::Null);
                self.on_notification(&m, &p);
                continue;
            }
            let same_id = msg.get("id") == Some(&Value::from(id));
            if same_id {
                if let Some(err) = msg.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed");
                    return Err(format!(
                        "{method}: {}",
                        m.chars().take(200).collect::<String>()
                    ));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// initialize → initialized → external login → thread/start.
    fn handshake(
        &mut self,
        turn: &PreparedTurn,
        deadline: Instant,
        auth: &setup::CachedAuth,
    ) -> Result<(), String> {
        let init_deadline = deadline.min(Instant::now() + CALL_TIMEOUT);
        self.call(
            "initialize",
            json!({
                "clientInfo": {"name": "gray-codex-sub", "version": env!("CARGO_PKG_VERSION")},
                // Unlocks dynamicTools and any other experimental field.
                "capabilities": {"experimentalApi": true},
            }),
            init_deadline,
        )?;
        self.send(&json!({"method": "initialized", "params": {}}))?;
        let mut login = json!({
            "type": "chatgptAuthTokens",
            "accessToken": auth.access_token,
        });
        if let Some(a) = &auth.account_id {
            login["chatgptAccountId"] = json!(a);
        }
        if let Some(p) = &auth.plan_type {
            login["chatgptPlanType"] = json!(p);
        }
        self.call("account/login/start", login, init_deadline)
            .map_err(|e| format!("external auth failed (re-run /connect codex?): {e}"))?;
        let mut params = json!({
            "cwd": self._stage.path().to_string_lossy(),
            // Codex's own tools may never ask (there is no human) and may
            // never write; the model is told they are disabled regardless.
            "approvalPolicy": "never",
            "sandbox": "read-only",
            // No rollout on disk — the pool keeps the thread in-process.
            "ephemeral": true,
        });
        if !turn.native_model.is_empty() {
            params["model"] = json!(turn.native_model);
            params["modelProvider"] = json!("openai");
        }
        if !turn.system.is_empty() {
            // Additive slot: Codex keeps its base agent contract (which is
            // what makes dynamicTools callable), the host prompt rides on
            // top in developer position.
            params["developerInstructions"] = json!(turn.system);
        }
        if !turn.dynamic_tools.is_empty() {
            params["dynamicTools"] = json!(turn.dynamic_tools);
        }
        self.call("thread/start", params, init_deadline)
            .and_then(|r| {
                r.get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .ok_or_else(|| "thread/start: no thread id".to_string())
            })
            .map(|id| {
                self.thread_id = id;
            })
    }

    /// First turn on a fresh thread: inject prior history (if any), then
    /// `turn/start` the final user-side segment.
    pub fn drive_fresh(
        &mut self,
        turn: &PreparedTurn,
        deadline: Instant,
        cancel: &Arc<AtomicBool>,
    ) -> Result<TurnResult, String> {
        self.reset_turn_state();
        let split = chat::fresh_split(&turn.input);
        let (history, tail) = turn.input.split_at(split);
        let mut inject = chat::to_inject_items(history);
        let mut inputs = chat::to_user_inputs(tail);
        if inputs.is_empty() {
            // Tail held no user text (pure tool outputs): inject them too
            // and nudge the model to continue its loop.
            inject = chat::to_inject_items(&turn.input);
            inputs = vec![json!({"type": "text", "text": chat::CONTINUE_NUDGE})];
        }
        if !inject.is_empty() {
            self.call(
                "thread/inject_items",
                json!({"threadId": self.thread_id, "items": inject}),
                deadline,
            )?;
        }
        self.turn_start(inputs, &turn.native_model, turn.effort.as_deref(), deadline)?;
        self.pump(deadline, cancel)
    }

    /// A pooled session claimed for continuation.
    pub fn drive(
        &mut self,
        claim: Claim,
        deadline: Instant,
        cancel: &Arc<AtomicBool>,
    ) -> Result<TurnResult, String> {
        self.reset_turn_state();
        match claim {
            Claim::Delta { tail, undelivered } => {
                let mut inputs = chat::to_user_inputs(&tail);
                if undelivered {
                    inputs.insert(0, json!({"type": "text", "text": chat::UNDELIVERED_NOTE}));
                }
                if inputs.is_empty() {
                    inputs = vec![json!({"type": "text", "text": chat::CONTINUE_NUDGE})];
                }
                let model = self.model.clone();
                let effort = self.effort.clone();
                self.turn_start(inputs, &model, effort.as_deref(), deadline)?;
            }
            Claim::Suspended { outputs } => {
                self.suspended = false;
                for item in &outputs {
                    let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                    if let Some(pos) = self.parked.iter().position(|p| p.call_id == call_id) {
                        let p = self.parked.remove(pos);
                        self.answer_call(p.request_id, item.get("output").unwrap_or(&Value::Null));
                    }
                }
            }
        }
        self.pump(deadline, cancel)
    }

    fn reset_turn_state(&mut self) {
        self.out.clear();
        self.items.clear();
        self.pending_error = None;
        self.current_turn_id = None;
        self.usage_turn_start = self.usage_total.clone();
    }

    fn turn_start(
        &mut self,
        input: Vec<Value>,
        model: &str,
        effort: Option<&str>,
        deadline: Instant,
    ) -> Result<(), String> {
        let mut params = json!({
            "threadId": self.thread_id,
            "input": input,
            // Each turn reasserts the no-write sandbox.
            "sandboxPolicy": {"type": "readOnly"},
        });
        if !model.is_empty() {
            params["model"] = json!(model);
        }
        if let Some(e) = effort {
            params["effort"] = json!(e);
        }
        self.call("turn/start", params, deadline)?;
        Ok(())
    }

    /// The turn ended: record what the host will echo back next time.
    pub fn absorb(&mut self, turn: &PreparedTurn, r: &TurnResult) {
        self.absorbed = turn.input.clone();
        if r.unseen {
            // Reply never reached the host: nothing will be echoed.
            self.reply_call_ids = Vec::new();
            self.reply_text = String::new();
        } else {
            self.reply_call_ids = r.call_ids();
            self.reply_text = r.text();
        }
        self.last_used = Instant::now();
        self.last_prompt_at = Instant::now();
    }

    /// Failed-but-settled turn: absorb the input so a retry continues
    /// this thread, and bar other conversations from claiming a child
    /// that already holds this transcript.
    pub fn absorb_failed(&mut self, turn: &PreparedTurn) {
        self.absorbed = turn.input.clone();
        self.reply_call_ids = Vec::new();
        self.reply_text = String::new();
        self.prompt_in_flight = false;
        self.suspended = false;
        self.parked.clear();
        self.last_used = Instant::now();
        self.last_prompt_at = Instant::now();
    }

    /// One cache-warming turn on an idle session. Parked calls can't
    /// appear (no tools in a keepalive prompt path that yields them — if
    /// they do, fail them and interrupt).
    fn keepalive(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let cancel = Arc::new(AtomicBool::new(false));
        self.reset_turn_state();
        let model = self.model.clone();
        let effort = self.effort.clone();
        let input = vec![json!({"type": "text",
            "text": "[automated cache refresh, not from the user: reply with only \"ok\"]"} )];
        if self
            .turn_start(input, &model, effort.as_deref(), deadline)
            .is_err()
        {
            self.closed = true;
            return;
        }
        match self.pump(deadline, &cancel) {
            Ok(mut r) => {
                // A keepalive must never suspend the session on calls.
                if !self.parked.is_empty() {
                    self.answer_parked_fail("(keepalive: calls unsupported)");
                    self.interrupt();
                    let _ = self.wait_settle(Instant::now() + INTERRUPT_GRACE);
                    self.suspended = false;
                    r.stop = "interrupted".into();
                }
                // Keepalive is invisible to the continuation contract:
                // `absorbed` still names the last real gray input.
            }
            Err(_) => {
                if self.prompt_in_flight {
                    self.closed = true;
                }
            }
        }
        self.last_used = Instant::now();
    }

    /// Terminate the child and drop the stage dir. Idempotent: `closed`
    /// also marks "doomed — never reuse", so a session flagged mid-turn
    /// still gets killed here.
    pub fn close(&mut self) {
        self.closed = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Idle sweep: kill sessions past IDLE_TIMEOUT (measured from the last
/// real prompt, so keepalives can't immortalize a dead conversation),
/// send a keepalive whenever upstream contact has gone stale — the
/// cache outlives a turn by less than IDLE_TIMEOUT, so a one-shot latch
/// would leave a live thread cold for most of its life — drop closed
/// handles.
fn sweep() {
    let mut pool = match POOL.lock() {
        Ok(p) => p,
        Err(e) => e.into_inner(),
    };
    let now = Instant::now();
    let mut keepalives: Vec<LiveSession> = Vec::new();
    let mut i = 0;
    while i < pool.len() {
        let s = &pool[i];
        let dead = s.closed || now.duration_since(s.last_prompt_at) > IDLE_TIMEOUT;
        if dead {
            let mut s = pool.remove(i);
            s.close();
            continue;
        }
        // Suspended sessions are mid-turn forever by design: the keepalive
        // path doesn't apply, only the idle timeout above.
        if !s.suspended && !s.prompt_in_flight && now.duration_since(s.last_used) > KEEPALIVE_AFTER
        {
            keepalives.push(pool.remove(i));
            continue;
        }
        i += 1;
    }
    drop(pool);
    for mut s in keepalives {
        s.keepalive();
        give_back(s);
    }
}

/// Start the background sweeper once; the sidecar lives forever so the
/// thread is intentionally never joined. A host that can't spare the
/// thread gets `Err` — the caller logs and runs without reaping.
pub fn start_reaper() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("codex-sub-reaper".into())
        .spawn(|| {
            loop {
                std::thread::sleep(REAPER_TICK);
                sweep();
            }
        })?;
    Ok(())
}
