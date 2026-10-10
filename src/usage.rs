//! `provider/usage`: Codex subscription quota from the ChatGPT
//! backend's `wham/usage` endpoint — the same feed Codex's own
//! `/status` rate-limit view reads. Bearer is the mirrored OAuth
//! access token (`~/.gray/codex-home/gray-auth.json`), refreshed
//! through the standard grant when stale; `chatgpt-account-id` rides
//! along exactly like the models call.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use gray_plugin::ProviderRpcError;

use crate::{oauth, setup};

/// The ChatGPT backend's usage route (verified: `plan_type` +
/// `rate_limit.primary_window/secondary_window` + `credits`).
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// A hung backend must not stall `/usage`.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Access-token runway below which we refresh before probing.
const REFRESH_SKEW_SECS: u64 = 60;

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

/// Window seconds → (id, label, kind). Codex's two buckets are a 5h
/// session window plus a weekly (plans) or monthly (free) secondary.
fn window_kind(secs: u64) -> (&'static str, &'static str, &'static str) {
    match secs {
        s if s <= 6 * 3600 => ("five_hour", "Session", "session"),
        s if s <= 8 * 86400 => ("seven_day", "Weekly", "weekly"),
        _ => ("monthly", "Monthly", "monthly"),
    }
}

fn window(v: &Value, id: &str, secondary: bool) -> Option<Value> {
    let secs = v.get("limit_window_seconds").and_then(Value::as_u64)?;
    let (wid, label, kind) = window_kind(secs);
    let label = if secondary {
        format!("{label} (secondary)")
    } else {
        label.to_string()
    };
    Some(json!({
        "id": format!("{id}_{wid}"),
        "label": label,
        "kind": kind,
        "duration_mins": secs / 60,
        "used_percent": v.get("used_percent").and_then(Value::as_f64),
        "resets_at": v.get("reset_at").and_then(Value::as_u64).map(iso),
    }))
}

/// `provider/usage` entry point: async — refresh + probe both await.
pub async fn handle() -> Result<Value, ProviderRpcError> {
    let mut auth = setup::read_cached().ok_or_else(|| {
        ProviderRpcError::Unavailable("no Codex login — connect `codex-login` first".into())
    })?;
    if !auth.access_fresh(now_secs(), REFRESH_SKEW_SECS) {
        let rt = auth.refresh_token.clone().ok_or_else(|| {
            ProviderRpcError::Unavailable(
                "Codex login has no refresh token — reconnect".into(),
            )
        })?;
        let set = oauth::refresh_cached(&oauth::OAuthConfig::default(), &rt).await?;
        auth = setup::CachedAuth {
            access_token: set.access_token,
            refresh_token: Some(set.refresh_token),
            account_id: Some(set.account_id),
            expires_at: Some(set.expires_at),
            plan_type: set.plan_type,
        };
        setup::store(&auth);
    }

    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|_| ProviderRpcError::Unavailable("http client init".into()))?;
    let resp = client
        .get(USAGE_URL)
        .bearer_auth(&auth.access_token)
        .header("chatgpt-account-id", auth.account_id.as_deref().unwrap_or_default())
        .header("originator", "gray")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| ProviderRpcError::Unavailable(format!("wham/usage: {e}")))?;
    if !resp.status().is_success() {
        return Err(ProviderRpcError::Unavailable(format!(
            "wham/usage HTTP {}",
            resp.status()
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| ProviderRpcError::Unavailable("wham/usage: bad JSON".into()))?;
    Ok(map_usage(&v, &auth))
}

fn map_usage(v: &Value, auth: &setup::CachedAuth) -> Value {
    let mut windows = Vec::new();
    let rl = v.get("rate_limit");
    if let Some(p) = rl.and_then(|r| r.get("primary_window")).filter(|w| !w.is_null()) {
        if let Some(w) = window(p, "primary", false) {
            windows.push(w);
        }
    }
    if let Some(s) = rl.and_then(|r| r.get("secondary_window")).filter(|w| !w.is_null()) {
        if let Some(w) = window(s, "secondary", true) {
            windows.push(w);
        }
    }

    let mut notes = Vec::new();
    if let Some(c) = v.get("credits") {
        if c.get("unlimited").and_then(Value::as_bool) == Some(true) {
            notes.push("unlimited credits".to_string());
        } else if c.get("has_credits").and_then(Value::as_bool) == Some(true) {
            if let Some(b) = c.get("balance").and_then(Value::as_f64) {
                notes.push(format!("credits balance ${b:.2}"));
            }
        }
    }
    if rl.and_then(|r| r.get("limit_reached")).and_then(Value::as_bool) == Some(true) {
        notes.push("limit reached".to_string());
    }

    // Plan: the response's plan_type wins; the JWT-cached one is the
    // fallback for a partial payload.
    let plan = v
        .get("plan_type")
        .and_then(Value::as_str)
        .or(auth.plan_type.as_deref())
        .map(|p| {
            let mut c = p.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_else(|| p.to_string())
        });

    json!({
        "available": true,
        "title": "Codex",
        "plan": plan,
        "windows": windows,
        "checked_at": iso(now_secs()),
        "note": if notes.is_empty() { Value::Null } else { json!(notes.join(" · ")) },
    })
}
