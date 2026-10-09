//! Chat turn over the loopback relay: the OpenAI Responses body the host
//! POSTs is answered by a pooled `codex app-server` child (see
//! [`crate::session`]) holding one Codex thread per conversation.
//!
//! The mapping (verified against codex-cli 0.154.0 over NDJSON stdio):
//! * Codex threads carry real Responses items. A fresh conversation does
//!   `thread/start` (developer text + `dynamicTools`), injects the prior
//!   transcript with `thread/inject_items`, then `turn/start`s the new
//!   user delta. A request whose history strictly extends what a pooled
//!   session last answered skips the rebuild entirely: same thread, only
//!   the delta.
//! * gray tools are the model's `dynamicTools`. When the model calls one,
//!   the app-server parks it as an `item/tool/call` server request; the
//!   relay answers the host with a `function_call` item and SUSPENDS —
//!   the Codex turn stays open. The next host request carries
//!   `function_call_output` items that resolve the parked calls, and the
//!   same turn resumes under the hood.
//! * `approvalPolicy: "never"` + `sandboxPolicy: readOnly` keep Codex's
//!   own tools from ever prompting or mutating (there is no config switch
//!   to unlist them; both are belt and suspenders since the model is
//!   told they are disabled).
//! * The relay speaks the OpenAI Responses SSE wire the host already
//!   streams, so no host changes are needed: the declared transport
//!   points at the per-turn relay URL and the host POSTs its standard
//!   body with the per-turn bearer.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::session;

/// Relay rejection when native retries past the single admitted request.
pub const ADMISSION_CONSUMED: &str = "CODEX_MODEL_ADMISSION_CONSUMED";

/// Prepended to a continuation delta whose previous reply never reached
/// the host.
pub(crate) const UNDELIVERED_NOTE: &str = "[Harness note] Your previous reply was interrupted by the user and never delivered; none of its tool calls ran.";

/// Nudge sent as the sole `turn/start` input on a fresh session whose
/// transcript tail is tool outputs: Codex user input carries no
/// tool-result item, so the injected history ends mid-loop and the model
/// just needs the word to continue.
pub(crate) const CONTINUE_NUDGE: &str = "[Harness note] The tool calls above were executed by the harness; their results are in the transcript. Continue.";

/// One translated turn: developer text, raw Responses input, tools, model.
pub struct PreparedTurn {
    /// Host `instructions` + leading system/developer message text: goes
    /// to `thread/start.developerInstructions`, never into the transcript.
    pub system: String,
    /// The request's raw `input` array: continuation matching keys off it.
    pub input: Vec<Value>,
    /// `dynamicTools` specs for `thread/start` (empty = no host tools).
    pub dynamic_tools: Vec<Value>,
    /// Stable fingerprint of `system` + `dynamic_tools`: a reused session
    /// only matches a request whose declared surface is identical.
    pub surface: String,
    pub native_model: String,
    /// `reasoning.effort` passthrough (`turn/start.effort`).
    pub effort: Option<String>,
}

/// Normalize a tool input schema: strip top-level `oneOf`/`allOf`/`anyOf`
/// and guarantee object schemas carry `properties`.
pub fn normalize_input_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    if let Some(obj) = out.as_object_mut() {
        for key in ["oneOf", "allOf", "anyOf"] {
            obj.remove(key);
        }
        obj.entry("type".to_string())
            .or_insert(Value::String("object".to_string()));
        if obj.get("type").and_then(Value::as_str) == Some("object")
            && !matches!(obj.get("properties"), Some(Value::Object(_)))
        {
            obj.insert("properties".to_string(), json!({}));
        }
    }
    out
}

/// Kind of a Responses input item: the `type` field, or "message" for the
/// EasyInputMessage short form (role present, type absent) the host emits.
fn item_kind(item: &Value) -> &str {
    match item.get("type").and_then(Value::as_str) {
        Some(k) => k,
        None if item.get("role").is_some() => "message",
        None => "",
    }
}

/// An input item the assistant side produced: the replayed echo of a
/// session's own answer (assistant message, its calls, reasoning carriers).
fn assistant_side(item: &Value) -> bool {
    match item_kind(item) {
        "function_call" | "reasoning" => true,
        "message" => item.get("role").and_then(Value::as_str) == Some("assistant"),
        _ => false,
    }
}

fn text_of(blocks: &Value) -> String {
    match blocks {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") | Some("input_text") | Some("output_text") => {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                }
                Some("input_image") => Some("[image]".to_string()),
                Some("input_file" | "input_video") => Some("[file omitted]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Translate an OpenAI Responses body into one prepared turn.
pub fn prepare_turn(body: &Value, model: &str) -> Result<PreparedTurn, String> {
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if input.is_empty() {
        return Err("history must end in a nonempty user/tool-result message".into());
    }
    let mut system_parts: Vec<String> = Vec::new();
    if !instructions.is_empty() {
        system_parts.push(instructions.to_string());
    }
    // Leading system/developer items fold into developerInstructions; one
    // appearing mid-history would silently reorder the transcript, so it's
    // a hard error rather than a wrong prompt.
    let mut seen_user_side = false;
    for item in &input {
        if item_kind(item) != "message" {
            continue;
        }
        match item.get("role").and_then(Value::as_str) {
            Some("system" | "developer") if !seen_user_side => {
                system_parts.push(text_of(item.get("content").unwrap_or(&Value::Null)));
            }
            Some("system" | "developer") => {
                return Err("system messages must precede conversation history".into());
            }
            Some("user") => seen_user_side = true,
            _ => {}
        }
    }
    let mut dynamic_tools: Vec<Value> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The operator's allowlist (`/codex tools`, default bash-only): only
    // passing tools reach the `dynamicTools` the turn advertises.
    let policy = crate::settings::ToolPolicy::load();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            // Responses tools are functions; anything else is host-owned.
            if t.get("type")
                .and_then(Value::as_str)
                .is_some_and(|k| k != "function")
            {
                continue;
            }
            // Filter before validating: a disallowed tool is invisible
            // here, so its name (valid or not) can never fail a turn.
            if !policy.allows(name) {
                continue;
            }
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                || !seen_names.insert(name.to_string())
            {
                return Err(format!(
                    "tool names must be unique ASCII identifiers: {name:?}"
                ));
            }
            dynamic_tools.push(json!({
                "type": "function",
                "name": name,
                "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                "inputSchema": normalize_input_schema(t.get("parameters").unwrap_or(&json!({}))),
            }));
        }
    }
    let last_kind = input.last().map(item_kind).unwrap_or("");
    if !matches!(last_kind, "message" | "function_call_output")
        || input
            .last()
            .and_then(|i| i.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
    {
        return Err(
            "history must end in a nonempty user/tool-result message; assistant prefill is unsupported"
                .into(),
        );
    }
    let system = system_parts.join("\n\n");
    // The reuse surface: developer text plus the exact tool manifest. A
    // changed toolset or system text must spawn fresh — `thread/start`
    // only takes them once.
    let surface = format!(
        "{}\u{0}{}",
        system,
        serde_json::to_string(&dynamic_tools).unwrap_or_default()
    );
    let effort = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(PreparedTurn {
        system,
        input,
        dynamic_tools,
        surface,
        native_model: model.to_string(),
        effort,
    })
}

/// Responses message content → Codex `UserInput` items: text and inline
/// `data:` images. Remote URLs are not fetched.
fn content_to_user_input(content: &Value, out: &mut Vec<Value>) {
    match content {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(json!({"type": "text", "text": s}));
            }
        }
        Value::Array(arr) => {
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            out.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("input_image") => {
                        let url = b.get("image_url").unwrap_or(&Value::Null);
                        let url = url
                            .as_str()
                            .or_else(|| url.get("url").and_then(Value::as_str));
                        if let Some(u) = url.filter(|u| u.starts_with("data:")) {
                            out.push(json!({"type": "image", "url": u}));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// `turn/start` `input` for user-role items (the delta of a continuation,
/// or the final user segment of a fresh spawn). Anything that renders to
/// nothing is skipped.
pub fn to_user_inputs(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        if item_kind(item) != "message" {
            continue;
        }
        if item.get("role").and_then(Value::as_str) == Some("user") {
            content_to_user_input(item.get("content").unwrap_or(&Value::Null), &mut out);
        }
    }
    out
}

/// One Responses `input` item → a canonical `ResponseItem` for
/// `thread/inject_items`. Reasoning carriers are dropped: they are opaque
/// to Codex and the real reasoning already lives in the thread upstream.
/// Anything unrecognized maps to `None` (caller skips it).
fn to_inject_item(item: &Value) -> Option<Value> {
    match item_kind(item) {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            // Leading system/developer text already rode into
            // `developerInstructions` (see prepare_turn); injecting it
            // again would double it and may fail role validation.
            if matches!(role, "system" | "developer") {
                return None;
            }
            let content = item.get("content").unwrap_or(&Value::Null);
            let part_type = match role {
                "assistant" => "output_text",
                _ => "input_text",
            };
            let mut parts: Vec<Value> = Vec::new();
            match content {
                Value::String(s) => {
                    if !s.is_empty() {
                        parts.push(json!({"type": part_type, "text": s}));
                    }
                }
                Value::Array(arr) => {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") | Some("input_text") | Some("output_text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    parts.push(json!({"type": part_type, "text": t}));
                                }
                            }
                            Some("input_image") => {
                                let url = b.get("image_url").unwrap_or(&Value::Null);
                                let url = url
                                    .as_str()
                                    .or_else(|| url.get("url").and_then(Value::as_str));
                                if let Some(u) = url.filter(|u| u.starts_with("data:")) {
                                    parts.push(json!({"type": "input_image", "image_url": u}));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if parts.is_empty() {
                return None;
            }
            Some(json!({"type": "message", "role": role, "content": parts}))
        }
        "function_call" => Some(json!({
            "type": "function_call",
            "call_id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
            "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
            "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or(""),
        })),
        "function_call_output" => Some(json!({
            "type": "function_call_output",
            "call_id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
            "output": item.get("output").cloned().unwrap_or(Value::Null),
        })),
        _ => None,
    }
}

/// Items for `thread/inject_items`, in order.
pub fn to_inject_items(items: &[Value]) -> Vec<Value> {
    items.iter().filter_map(to_inject_item).collect()
}

/// Where a fresh session's `input` splits: everything before the final
/// run of user-side items is history to inject; the tail drives the
/// first `turn/start`. A tail that is pure tool outputs can't be a turn
/// input (Codex takes only user items there), so the caller nudges.
pub(crate) fn fresh_split(input: &[Value]) -> usize {
    let mut i = input.len();
    while i > 0 && !assistant_side(&input[i - 1]) {
        i -= 1;
    }
    i
}

/// Whether every tail item is a `function_call_output` answering a parked
/// call: the suspended-session continuation shape.
pub(crate) fn outputs_for(tail: &[Value], parked: &[String]) -> bool {
    !tail.is_empty()
        && tail.iter().all(|i| {
            item_kind(i) == "function_call_output"
                && i.get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|c| parked.iter().any(|p| p == c))
        })
}

/// Strict-continuation check: `input` is `absorbed` plus the echo of the
/// session's own last answer plus a non-assistant tail. Returns the tail
/// (still raw items — Codex takes real items, not rendered text). Miss
/// reasons feed the CODEX_SUB_DEBUG trace: `prefix` (history diverged),
/// `echo` (the replayed answer isn't what this session sent),
/// `empty_delta` (nothing new to ask, or an assistant item sits in the
/// new tail, which means the history mid-edited a turn).
///
/// The host echoes our answer as `{"role":"assistant","content":<text>}`
/// (only when the text is non-empty) then one `{"type":"function_call"}`
/// item per call; reasoning items may interleave and carry nothing here.
pub(crate) fn continuation<'a>(
    absorbed: &[Value],
    reply_call_ids: &[String],
    reply_text: &str,
    input: &'a [Value],
) -> Result<(&'a [Value], bool), &'static str> {
    if input.len() <= absorbed.len() || !input.starts_with(absorbed) {
        return Err("prefix");
    }
    let rest = &input[absorbed.len()..];
    let mut i = 0;
    let mut call_ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    while i < rest.len() && assistant_side(&rest[i]) {
        let item = &rest[i];
        match item_kind(item) {
            "function_call" => call_ids.push(
                item.get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            "message" => texts.push(text_of(item.get("content").unwrap_or(&Value::Null))),
            _ => {}
        }
        i += 1;
    }
    // An empty echo zone against a non-empty recorded reply: the reply
    // never reached gray (interrupted turn, client gone before delivery).
    // The child still holds it, so continue with a note instead of
    // re-billing the whole prefix on a fresh session.
    let undelivered = i == 0 && (!reply_call_ids.is_empty() || !reply_text.trim().is_empty());
    if !undelivered
        && (call_ids.as_slice() != reply_call_ids || texts.join("\n").trim() != reply_text.trim())
    {
        return Err("echo");
    }
    let tail = &rest[i..];
    if tail.is_empty() || tail.iter().any(assistant_side) {
        return Err("empty_delta");
    }
    Ok((tail, undelivered))
}

/// Cumulative thread token usage as reported by
/// `thread/tokenUsage/updated` (`total` bucket). A turn's usage is the
/// delta between turn end and turn start.
#[derive(Default, Clone)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
    pub cached_tokens: usize,
    pub cache_write_tokens: usize,
}

impl Usage {
    pub fn from_token_usage(v: &Value) -> Option<Usage> {
        let total = v.get("total").unwrap_or(v);
        let get = |k: &str| total.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        let u = Usage {
            input_tokens: get("inputTokens"),
            output_tokens: get("outputTokens"),
            total_tokens: get("totalTokens"),
            cached_tokens: get("cachedInputTokens"),
            cache_write_tokens: get("cacheWriteInputTokens"),
        };
        (u.total_tokens > 0 || u.output_tokens > 0).then_some(u)
    }

    /// `self - start`, saturating (usage counters only move forward).
    pub fn delta(&self, start: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_sub(start.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(start.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(start.total_tokens),
            cached_tokens: self.cached_tokens.saturating_sub(start.cached_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(start.cache_write_tokens),
        }
    }
}

/// One output item the fold emits, in turn order.
pub enum OutItem {
    /// Assistant message text (item id, full text).
    Message { id: String, text: String },
    /// Reasoning summary (item id, text).
    Reasoning { id: String, text: String },
    /// A parked dynamic tool call (call id, tool name, arguments JSON string).
    Call {
        call_id: String,
        name: String,
        args: String,
    },
}

/// What one Codex turn segment produced for the host.
pub struct TurnResult {
    /// Emittable items in emission order (new since the last gray turn).
    pub items: Vec<OutItem>,
    pub usage: Usage,
    /// "completed" | "incomplete:…" | "failed:…" | "interrupted".
    pub stop: String,
    /// The relay client was already gone when this turn settled: the
    /// reply was produced but never delivered, so the session records an
    /// empty echo for it (see `LiveSession::absorb`).
    pub unseen: bool,
}

impl TurnResult {
    /// All emitted `call_id`s in order — the echo-check list.
    pub fn call_ids(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|i| match i {
                OutItem::Call { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Concatenated assistant text, trimmed — the echo-check text.
    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|i| match i {
                OutItem::Message { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Run one turn: continue a pooled session (idle → `turn/start` delta;
/// suspended → answer parked calls) when this request's history strictly
/// extends what it last answered, else spawn `codex app-server`, drive
/// initialize + external-auth login + thread/start (+ inject for prior
/// history) and `turn/start` the delta. `timeout` is the whole-turn
/// budget; `cancel` is set when the relay client went away mid-turn.
///
/// Pooling rules mirror Devin's: a failure whose turn still settled
/// upstream returns the session (`prompt_in_flight` cleared); only a
/// turn left running — dead wire, unsettled interrupt — closes it.
pub fn run_turn(
    turn: &PreparedTurn,
    cancel: &Arc<AtomicBool>,
    timeout: Duration,
) -> Result<TurnResult, String> {
    let deadline = Instant::now() + timeout;
    let (claimed, reason) = session::take(turn, cancel);
    match claimed {
        Some((mut s, claim)) => {
            let mode = format!("reuse:{}", claim.kind());
            match s.drive(claim, deadline, cancel) {
                Ok(r) => {
                    s.absorb(turn, &r);
                    trace_turn(&mode, &s.debug_label(), turn.input.len(), Some(&r), None);
                    session::give_back(s);
                    Ok(r)
                }
                Err(e) => {
                    trace_turn(&mode, &s.debug_label(), turn.input.len(), None, Some(&e));
                    if s.prompt_in_flight {
                        s.close();
                    } else {
                        session::give_back(s);
                    }
                    Err(e)
                }
            }
        }
        None => {
            let mut s = match session::spawn(turn, deadline, cancel) {
                Ok(s) => s,
                Err(e) => {
                    trace_turn(
                        &format!("fresh:{reason}"),
                        "-",
                        turn.input.len(),
                        None,
                        Some(&e),
                    );
                    return Err(e);
                }
            };
            let mode = format!("fresh:{reason}");
            match s.drive_fresh(turn, deadline, cancel) {
                Ok(r) => {
                    s.absorb(turn, &r);
                    trace_turn(&mode, &s.debug_label(), turn.input.len(), Some(&r), None);
                    session::give_back(s);
                    Ok(r)
                }
                Err(e) => {
                    trace_turn(&mode, &s.debug_label(), turn.input.len(), None, Some(&e));
                    if s.prompt_in_flight {
                        s.close();
                    } else {
                        // The failed turn settled upstream: absorb the
                        // input so a later request can continue this
                        // thread — and no other conversation can claim a
                        // child already holding this transcript.
                        s.absorb_failed(turn);
                        session::give_back(s);
                    }
                    Err(e)
                }
            }
        }
    }
}

/// One line per turn when `CODEX_SUB_DEBUG` is set: mode, thread, prompt
/// size and usage — never prompt content. Appends (mode 0600) to
/// `<tempdir>/codex-sub-<pid>.log`.
pub(crate) fn trace_turn(
    mode: &str,
    label: &str,
    input_items: usize,
    r: Option<&TurnResult>,
    err: Option<&str>,
) {
    if std::env::var_os("CODEX_SUB_DEBUG").is_none() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default();
    let (usage, stop) = match r {
        Some(r) => (
            format!(
                "{}/{}/{}",
                r.usage.input_tokens, r.usage.cached_tokens, r.usage.output_tokens
            ),
            r.stop.clone(),
        ),
        None => (
            "0/0/0".to_string(),
            match err {
                Some(e) => format!("error:{}", e.chars().take(120).collect::<String>()),
                None => "error".to_string(),
            },
        ),
    };
    let line = format!(
        "{ts}\t{mode}\tsession={label}\tinput_items={input_items}\tusage={usage}\tstop={stop}\n"
    );
    let path = std::env::temp_dir().join(format!("codex-sub-{}.log", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Fold a completed turn into a Responses SSE stream.
pub fn fold_result(r: &TurnResult, model: &str) -> Result<Vec<u8>, String> {
    let mut sse = Vec::new();
    let emit = |sse: &mut Vec<u8>, payload: &Value| {
        sse.extend_from_slice(b"data: ");
        sse.extend_from_slice(payload.to_string().as_bytes());
        sse.extend_from_slice(b"\n\n");
    };
    let resp_id = format!("resp_{}", rand_hex(12));
    emit(
        &mut sse,
        &json!({"type": "response.created",
            "response": {"id": resp_id, "model": model, "status": "in_progress"}}),
    );
    let mut output_index = 0usize;
    for item in &r.items {
        match item {
            OutItem::Reasoning { id, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                let it = json!({"type": "reasoning", "id": id,
                    "summary": [{"type": "summary_text", "text": text}]});
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.added",
                        "output_index": output_index, "item": it}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.done",
                        "output_index": output_index, "item": it}),
                );
                output_index += 1;
            }
            OutItem::Message { id, text } => {
                if text.is_empty() {
                    continue;
                }
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {"type": "message", "id": id, "role": "assistant",
                            "status": "in_progress",
                            "content": [{"type": "output_text", "text": "", "annotations": []}]}}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.output_text.delta",
                        "output_index": output_index, "item_id": id,
                        "content_index": 0, "delta": text}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.output_text.done",
                        "output_index": output_index, "item_id": id,
                        "content_index": 0, "text": text}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.done",
                        "output_index": output_index,
                        "item": {"type": "message", "id": id, "role": "assistant",
                            "status": "completed",
                            "content": [{"type": "output_text", "text": text, "annotations": []}]}}),
                );
                output_index += 1;
            }
            OutItem::Call {
                call_id,
                name,
                args,
            } => {
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {"type": "function_call", "id": call_id,
                            "call_id": call_id, "name": name, "arguments": args,
                            "status": "in_progress"}}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.function_call_arguments.done",
                        "output_index": output_index, "item_id": call_id,
                        "call_id": call_id, "name": name, "arguments": args}),
                );
                emit(
                    &mut sse,
                    &json!({"type": "response.output_item.done",
                        "output_index": output_index,
                        "item": {"type": "function_call", "id": call_id,
                            "call_id": call_id, "name": name, "arguments": args,
                            "status": "completed"}}),
                );
                output_index += 1;
            }
        }
    }
    let usage_val = json!({"input_tokens": r.usage.input_tokens,
        "output_tokens": r.usage.output_tokens,
        "total_tokens": r.usage.total_tokens,
        "input_tokens_details": {"cached_tokens": r.usage.cached_tokens,
            "cache_creation_tokens": r.usage.cache_write_tokens}});
    let (status, incomplete) = match r.stop.as_str() {
        s if s.starts_with("incomplete:") => (
            "incomplete",
            json!({"reason": s.trim_start_matches("incomplete:")}),
        ),
        // Responses has no "interrupted" status — closest is cancelled.
        "interrupted" => ("cancelled", Value::Null),
        s => (s, Value::Null),
    };
    emit(
        &mut sse,
        &json!({"type": "response.completed",
            "response": {"id": resp_id, "model": model, "status": status,
                "incomplete_details": incomplete, "usage": usage_val}}),
    );
    sse.extend_from_slice(b"data: [DONE]\n\n");
    Ok(sse)
}

pub fn rand_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut s = format!("{t:08x}{:08x}", std::process::id());
    while s.len() < n {
        s.push('0');
    }
    s[..n].to_string()
}
