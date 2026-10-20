//! `/codex status` — one glanceable panel: CLI presence, who is signed
//! in, plan, quota windows. Same convention `nous-sub status` set:
//! key / acct / plan / windows. Renders the same card `usage::handle()`
//! returns, so the panel and the host's `/usage` view never disagree.
use crate::{appserver, setup, usage};
use serde_json::{Value, json};
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
fn account_line(account: &Value) -> String {
    match account.get("account") {
        Some(a) if !a.is_null() => match a.get("type").and_then(Value::as_str) {
            Some("chatgpt") => {
                let email = a.get("email").and_then(Value::as_str).unwrap_or("?");
                let plan = a.get("planType").and_then(Value::as_str).unwrap_or("?");
                format!("chatgpt {email} · plan {plan}")
            }
            Some("apiKey") => "api key".to_string(),
            Some(other) => other.to_string(),
            None => "signed in".to_string(),
        },
        _ => "none — run `codex login`".to_string(),
    }
}
pub fn text() -> String {
    let mut out = "Codex (ChatGPT subscription via Codex CLI)".to_string();
    let probed = appserver::with_client(|c| {
        let account = c.call("account/read", json!({}))?;
        // A failed usage read must not kill the status panel.
        let limits = c
            .call("account/rateLimits/read", json!({}))
            .unwrap_or(Value::Null);
        Ok((account, limits))
    });
    match &probed {
        Err(appserver::Error::NotInstalled) => {
            out.push_str(&format!("\n  cli:   {}", setup::INSTALL_HINT));
        }
        Err(e) => out.push_str(&format!("\n  probe: {e}")),
        Ok((account, limits)) => {
            let signed_in = account.get("account").is_some_and(|a| !a.is_null());
            out.push_str(&format!("\n  login: {}", account_line(account)));
            if signed_in {
                let card = usage::card(account, limits);
                if let Some(plan) = card.get("plan").and_then(Value::as_str) {
                    out.push_str(&format!("\n  plan:  {plan}"));
                }
                if let Some(wins) = card.get("windows").and_then(Value::as_array) {
                    for w in wins {
                        if let Some(line) = window_line(w) {
                            out.push('\n');
                            out.push_str(&line);
                        }
                    }
                }
                if let Some(note) = card.get("note").and_then(Value::as_str)
                    && !note.is_empty()
                {
                    out.push_str(&format!("\n  note:  {note}"));
                }
            }
        }
    }
    let pool = crate::accounts::pool();
    if pool.iter().any(|a| !a.implicit) {
        let s = crate::accounts::serving();
        out.push_str(&format!(
            "\n  acct:  {} (serving, pool of {})",
            s.label,
            pool.len()
        ));
    }
    out.push_str(&format!(
        "\n  home:  {}",
        crate::accounts::serving_home().display()
    ));
    if setup::legacy_auth_file().is_file() {
        out.push_str(
            "\n  note:  retired credential mirror gray-auth.json still on disk — \
             revoke or delete it to clear",
        );
    }
    out
}
