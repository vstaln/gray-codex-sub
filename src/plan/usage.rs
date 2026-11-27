use gray_plugin::{ProviderRpcError, ProviderRpcFailure};
use serde_json::{Value, json};

use super::oauth::read_bounded;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const USAGE_BODY_LIMIT: usize = 256 * 1024;

pub async fn fetch(
    http: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Value, ProviderRpcError> {
    let url = usage_url();
    let mut request = http
        .get(url)
        .bearer_auth(access_token)
        .header("Accept", "application/json");
    if let Some(id) = account_id.filter(|id| !id.is_empty()) {
        request = request.header("chatgpt-account-id", id);
    }
    let response = request.send().await.map_err(|_| unavailable())?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ProviderRpcError::Rpc(ProviderRpcFailure {
            code: "unauthorized".to_string(),
            message: "ChatGPT rejected this sign-in for plan usage; run /connect again."
                .to_string(),
            retryable: false,
            terminal: true,
        }));
    }
    if !status.is_success() {
        return Err(unavailable());
    }
    let body = read_bounded(response, USAGE_BODY_LIMIT)
        .await
        .ok_or_else(unavailable)?;
    let payload: Value = serde_json::from_slice(&body).map_err(|_| unavailable())?;
    Ok(card(&payload))
}

fn usage_url() -> &'static str {
    USAGE_URL
}

fn card(payload: &Value) -> Value {
    let limit = payload.get("rate_limit").filter(|v| !v.is_null());
    let mut windows = Vec::new();
    if let Some(window) = limit
        .and_then(|l| l.get("primary_window"))
        .filter(|v| !v.is_null())
        .and_then(|w| window(w, "primary"))
    {
        windows.push(window);
    }
    if let Some(window) = limit
        .and_then(|l| l.get("secondary_window"))
        .filter(|v| !v.is_null())
        .and_then(|w| window(w, "secondary"))
    {
        windows.push(window);
    }
    json!({
        "available": true,
        "title": "ChatGPT",
        "plan": payload.get("plan_type").and_then(Value::as_str).map(capitalize),
        "windows": windows,
        "checked_at": iso(super::oauth::now_secs()),
        "note": Value::Null,
    })
}

fn window(w: &Value, which: &str) -> Option<Value> {
    let used = w.get("used_percent").and_then(Value::as_f64)?;
    let seconds = w.get("limit_window_seconds").and_then(Value::as_u64);
    let minutes = seconds.map(|s| s.div_ceil(60));
    let (id, label, kind) = window_kind(minutes);
    Some(json!({
        "id": format!("{which}_{id}"),
        "label": label,
        "kind": kind,
        "duration_mins": minutes,
        "used_percent": used,
        "resets_at": w.get("reset_at").and_then(Value::as_u64).map(iso),
    }))
}

fn window_kind(minutes: Option<u64>) -> (&'static str, &'static str, &'static str) {
    match minutes {
        Some(m) if m <= 6 * 60 => ("five_hour", "Session", "session"),
        Some(m) if m <= 8 * 24 * 60 => ("seven_day", "Weekly", "weekly"),
        Some(_) => ("monthly", "Monthly", "monthly"),
        None => ("window", "Usage", "other"),
    }
}

fn capitalize(p: &str) -> String {
    let mut c = p.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect::<String>())
        .unwrap_or_else(|| p.to_string())
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

fn unavailable() -> ProviderRpcError {
    ProviderRpcError::Unavailable("ChatGPT plan usage is unavailable right now".into())
}

pub fn iso_now() -> String {
    iso(super::oauth::now_secs())
}
