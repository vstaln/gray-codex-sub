//! `/codex status` — one glanceable panel: mirrored login, plan, quota
//! windows. Same convention `nous-sub status` set: key / acct / plan /
//! windows. Renders `usage::handle()`'s ProviderUsageLimits, so the panel
//! and the host's `/usage` view can never disagree.

use serde_json::Value;

use crate::{setup, usage};

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_left(expires_at: Option<u64>) -> String {
    let Some(exp) = expires_at else {
        return String::new();
    };
    let now = now_secs() as i64;
    let left = exp as i64 - now;
    let t = if left <= 0 {
        "expired".to_string()
    } else if left < 90 {
        format!("{left}s left")
    } else if left < 5400 {
        format!("{}m left", left / 60)
    } else {
        format!("{}h{}m left", left / 3600, left % 3600 / 60)
    };
    format!(" · {t}")
}

fn window_line(w: &Value) -> Option<String> {
    let label = w.get("label")?.as_str()?;
    let mut line = format!("  {label}:");
    if let Some(p) = w.get("used_percent").and_then(Value::as_f64) {
        line.push_str(&format!(" {p:.0}% used"));
    }
    if let Some(r) = w.get("resets_at").and_then(Value::as_str) {
        line.push_str(&format!(" · resets {r}"));
    }
    Some(line)
}

pub async fn text() -> String {
    let mut out = "Codex backend (unofficial)".to_string();
    let auth = setup::read_cached();
    match &auth {
        None => out.push_str("\n  login: none — /connect → Codex → codex-login"),
        Some(a) => {
            out.push_str(&format!(
                "\n  login: gray-auth.json (oauth){}",
                fmt_left(a.expires_at)
            ));
            if let Some(id) = a.account_id.as_deref() {
                out.push_str(&format!("\n  acct:  chatgpt {}…", &id[..id.len().min(8)]));
            }
        }
    }
    match usage::handle().await {
        Ok(v) => {
            if let Some(plan) = v.get("plan").and_then(Value::as_str) {
                out.push_str(&format!("\n  plan:  {plan}"));
            }
            if let Some(wins) = v.get("windows").and_then(Value::as_array) {
                for w in wins {
                    if let Some(line) = window_line(w) {
                        out.push('\n');
                        out.push_str(&line);
                    }
                }
            }
            if let Some(note) = v.get("note").and_then(Value::as_str)
                && !note.is_empty()
            {
                out.push_str(&format!("\n  note:  {note}"));
            }
        }
        Err(e) => out.push_str(&format!("\n  usage: ({e})")),
    }
    out
}
