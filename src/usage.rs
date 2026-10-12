//! `provider/usage`: Codex subscription quota from the app-server's own
//! `account/rateLimits/read` — the same read Codex's rate-limit view
//! uses. Plan comes from `account/read`. No HTTP and no tokens: the
//! CLI's stored login answers everything.
use crate::{appserver, setup};
use gray_plugin::ProviderRpcError;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn iso(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
/// Window minutes → (id, label, kind). Codex's two buckets are a 5h
/// session window plus a weekly (plans) or monthly (free) secondary.
fn window_kind(mins: u64) -> (&'static str, &'static str, &'static str) {
    match mins {
        m if m <= 6 * 60 => ("five_hour", "Session", "session"),
        m if m <= 8 * 24 * 60 => ("seven_day", "Weekly", "weekly"),
        _ => ("monthly", "Monthly", "monthly"),
    }
}
/// One `RateLimitWindow` ({usedPercent, windowDurationMins, resetsAt})
/// → a card row. A window without a duration is skipped.
fn window(v: &Value, id: &str, secondary: bool) -> Option<Value> {
    let mins = v.get("windowDurationMins").and_then(Value::as_u64)?;
    let (wid, label, kind) = window_kind(mins);
    let label = if secondary {
        format!("{label} (secondary)")
    } else {
        label.to_string()
    };
    Some(json!({
        "id": format!("{id}_{wid}"),
        "label": label,
        "kind": kind,
        "duration_mins": mins,
        "used_percent": v.get("usedPercent").and_then(Value::as_f64),
        "resets_at": v.get("resetsAt").and_then(Value::as_u64).map(iso),
    }))
}
fn capitalize(p: &str) -> String {
    let mut c = p.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect::<String>())
        .unwrap_or_else(|| p.to_string())
}
/// ProviderUsageLimits-shaped card from an `account/read` result and a
/// `account/rateLimits/read` result. Pure — `status` renders the same
/// card without a second probe.
pub fn card(account: &Value, limits: &Value) -> Value {
    let rl = limits.get("rateLimits").cloned().unwrap_or(Value::Null);
    let mut windows = Vec::new();
    if let Some(p) = rl.get("primary").filter(|w| !w.is_null())
        && let Some(w) = window(p, "primary", false)
    {
        windows.push(w);
    }
    if let Some(s) = rl.get("secondary").filter(|w| !w.is_null())
        && let Some(w) = window(s, "secondary", true)
    {
        windows.push(w);
    }
    let mut notes = Vec::new();
    if let Some(c) = rl.get("credits") {
        if c.get("unlimited").and_then(Value::as_bool) == Some(true) {
            notes.push("unlimited credits".to_string());
        } else if c.get("hasCredits").and_then(Value::as_bool) == Some(true)
            && let Some(b) = c.get("balance").and_then(Value::as_str)
        {
            notes.push(format!("credits balance {b}"));
        }
    }
    if rl.get("rateLimitReachedType").is_some_and(|t| !t.is_null()) {
        notes.push("limit reached".to_string());
    }
    // Plan: the snapshot's planType wins; the account's is the fallback
    // for a partial payload.
    let plan = rl
        .get("planType")
        .and_then(Value::as_str)
        .or_else(|| {
            account
                .get("account")
                .and_then(|a| a.get("planType"))
                .and_then(Value::as_str)
        })
        .map(capitalize);
    json!({
        "available": true,
        "title": "Codex",
        "plan": plan,
        "windows": windows,
        "checked_at": iso(now_secs()),
        "note": if notes.is_empty() { Value::Null } else { json!(notes.join(" · ")) },
    })
}
/// `provider/usage` entry point: one probe client, two reads.
pub fn handle() -> Result<Value, ProviderRpcError> {
    let (account, limits) = appserver::with_client(|c| {
        let account = c.call("account/read", json!({}))?;
        let limits = c.call("account/rateLimits/read", json!({}))?;
        Ok((account, limits))
    })
    .map_err(ProviderRpcError::from)?;
    if account.get("account").is_none_or(Value::is_null) {
        return Err(ProviderRpcError::Unavailable(setup::LOGIN_HINT.into()));
    }
    Ok(card(&account, &limits))
}
