use std::path::{Path, PathBuf};

use base64::Engine;
use gray_core::credential::CredentialMaterial;
use serde_json::{Value, json};

pub struct Account {
    pub label: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub subject: Option<String>,
    pub email: Option<String>,
    pub scope: Option<String>,
    pub account_id: Option<String>,
    pub expires_at: Option<u64>,
    pub pinned: bool,
    pub cooling: bool,
}

pub struct PoolView {
    pub strategy: String,
    pub selected: Option<String>,
    pub accounts: Vec<Account>,
}

pub fn gray_home() -> Option<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
}

fn auth_path() -> Option<PathBuf> {
    gray_home().map(|h| h.join("auth.json"))
}

pub fn has_plan_pool() -> bool {
    let Some(path) = auth_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(store) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    store.get(super::auth_ref()).is_some()
}

pub fn pool_view() -> PoolView {
    let now = super::oauth::now_secs();
    let Some(path) = auth_path() else {
        return PoolView {
            strategy: "fill_first".to_string(),
            selected: None,
            accounts: Vec::new(),
        };
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return PoolView {
            strategy: "fill_first".to_string(),
            selected: None,
            accounts: Vec::new(),
        };
    };
    let Ok(store) = serde_json::from_str::<Value>(&text) else {
        return PoolView {
            strategy: "fill_first".to_string(),
            selected: None,
            accounts: Vec::new(),
        };
    };
    let Some(pool) = store.get(super::auth_ref()) else {
        return PoolView {
            strategy: "fill_first".to_string(),
            selected: None,
            accounts: Vec::new(),
        };
    };
    let strategy = pool
        .get("strategy")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("fill_first")
        .to_string();
    let selected = pool
        .get("selected")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let accounts = pool
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let label = a.get("label").and_then(Value::as_str)?.to_string();
            let credential = a.get("credential")?;
            let secrets = credential.get("secrets")?;
            let access_token = secrets.get("access_token")?.as_str()?.to_string();
            let refresh_token = secrets
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_string);
            let meta = credential.get("metadata");
            let client_id = meta
                .and_then(|m| m.get("client_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let subject = meta
                .and_then(|m| m.get("subject"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let email = a
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| meta.and_then(|m| m.get("email")).and_then(Value::as_str))
                .map(str::to_string);
            let scope = meta
                .and_then(|m| m.get("scope"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let account_id = meta
                .and_then(|m| m.get("account_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let expires_at = credential.get("expires_at").and_then(Value::as_u64);
            let cooldown_until = a.get("cooldown_until").and_then(Value::as_u64).unwrap_or(0);
            let pinned = selected.as_deref() == Some(label.as_str());
            Some(Account {
                label,
                access_token,
                refresh_token,
                client_id,
                subject,
                email,
                scope,
                account_id,
                expires_at,
                pinned,
                cooling: cooldown_until > now,
            })
        })
        .collect();
    PoolView {
        strategy,
        selected,
        accounts,
    }
}

pub fn accounts() -> Vec<Account> {
    pool_view().accounts
}

fn mutate_pool<T>(f: impl FnOnce(&mut Value) -> Result<T, String>) -> Result<T, String> {
    let path = auth_path().ok_or("cannot resolve GRAY_HOME")?;
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut store: Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let binding = crate::manifest::plan_provider()
        .profile_binding("chatgpt-plan")
        .unwrap_or_default();
    if store.get(super::auth_ref()).is_none() {
        store[super::auth_ref()] = json!({
            "version": 2,
            "plugin": "codex-sub",
            "provider": "chatgpt",
            "auth_method": "chatgpt-plan",
            "strategy": "fill_first",
            "selected": Value::Null,
            "accounts": [],
        });
    }
    let pool = store
        .get_mut(super::auth_ref())
        .ok_or("missing chatgpt-plan pool")?;
    if pool.get("profile_binding").is_none() && !binding.is_empty() {
        let _ = &binding;
    }
    let out = f(pool)?;
    write_private_json(&path, &store)?;
    Ok(out)
}

fn write_private_json(path: &Path, val: &Value) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| format!("tempfile in {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(val).map_err(|e| e.to_string())?;
    use std::io::Write;
    tmp.write_all(&bytes).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(path)
        .map_err(|e| format!("persist {}: {}", path.display(), e.error))?;
    Ok(())
}

pub fn update_credential(label: &str, material: &CredentialMaterial) -> Result<(), String> {
    let cred_val = serde_json::to_value(material).map_err(|e| e.to_string())?;
    mutate_pool(|pool| {
        if let Some(arr) = pool.get_mut("accounts").and_then(Value::as_array_mut)
            && let Some(item) = arr
                .iter_mut()
                .find(|a| a.get("label").and_then(Value::as_str) == Some(label))
        {
            item["credential"] = cred_val;
            if let Some(email) = material.metadata.get("email") {
                item["email"] = json!(email);
            }
        }
        Ok(())
    })
}

pub fn upsert_account(label: &str, material: &CredentialMaterial) -> Result<String, String> {
    let binding = crate::manifest::plan_provider()
        .profile_binding("chatgpt-plan")
        .map_err(|e| e.to_string())?;
    let email = material.metadata.get("email").cloned();
    let cred_val = serde_json::to_value(material).map_err(|e| e.to_string())?;
    mutate_pool(|pool| {
        if pool.get("accounts").and_then(Value::as_array).is_none() {
            pool["accounts"] = json!([]);
        }
        let arr = pool
            .get_mut("accounts")
            .and_then(Value::as_array_mut)
            .ok_or("accounts is not an array")?;
        if let Some(existing) = arr
            .iter_mut()
            .find(|a| a.get("label").and_then(Value::as_str) == Some(label))
        {
            existing["credential"] = cred_val;
            existing["profile_binding"] = json!(binding);
            existing["cooldown_until"] = json!(0);
            if let Some(e) = &email {
                existing["email"] = json!(e);
            }
        } else {
            let mut entry = json!({
                "label": label,
                "profile_binding": binding,
                "credential": cred_val,
                "added_at": super::oauth::now_secs(),
                "cooldown_until": 0,
                "request_count": 0,
                "last_used_at": 0,
            });
            if let Some(e) = &email {
                entry["email"] = json!(e);
            }
            arr.push(entry);
        }
        if pool.get("selected").is_none_or(Value::is_null) {
            pool["selected"] = json!(label);
        }
        Ok(email.unwrap_or_else(|| "signed in".to_string()))
    })
}

pub fn set_selected(want: Option<&str>) -> Result<Option<String>, String> {
    mutate_pool(|pool| {
        if let Some(label) = want {
            let exists = pool
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|arr| {
                    arr.iter()
                        .any(|a| a.get("label").and_then(Value::as_str) == Some(label))
                });
            if !exists {
                return Err(format!("no account {label:?}"));
            }
            pool["selected"] = json!(label);
            Ok(Some(label.to_string()))
        } else {
            pool["selected"] = Value::Null;
            Ok(None)
        }
    })
}

pub fn remove_account(label: &str) -> Result<(), String> {
    mutate_pool(|pool| {
        let arr = pool
            .get_mut("accounts")
            .and_then(Value::as_array_mut)
            .ok_or("no accounts")?;
        let before = arr.len();
        arr.retain(|a| a.get("label").and_then(Value::as_str) != Some(label));
        if arr.len() == before {
            return Err(format!("no account {label:?}"));
        }
        if pool.get("selected").and_then(Value::as_str) == Some(label) {
            pool["selected"] = Value::Null;
        }
        Ok(())
    })
}

pub fn set_strategy(strategy: &str) -> Result<String, String> {
    if !matches!(strategy, "fill_first" | "least_used" | "round_robin") {
        return Err(format!("unknown strategy {strategy:?}"));
    }
    mutate_pool(|pool| {
        pool["strategy"] = json!(strategy);
        Ok(strategy.to_string())
    })
}

pub fn find_codex_cli_token(label: &str, email: Option<&str>) -> Option<(String, Option<String>)> {
    let mut candidates = Vec::new();
    if let Some(gh) = gray_home() {
        candidates.push(
            gh.join("codex-sub")
                .join("accounts")
                .join(label)
                .join("auth.json"),
        );
        if let Ok(rd) = std::fs::read_dir(gh.join("codex-sub").join("accounts")) {
            for ent in rd.flatten() {
                candidates.push(ent.path().join("auth.json"));
            }
        }
        candidates.push(gh.join("codex-home").join("auth.json"));
    }
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(h.join(".codex").join("auth.json"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(tokens) = doc.get("tokens") else {
            continue;
        };
        let Some(access) = tokens.get("access_token").and_then(Value::as_str) else {
            continue;
        };
        let acct_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tok_email = jwt_email(access).or_else(|| {
            tokens
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(jwt_email)
        });
        if let Some(want_email) = email {
            if tok_email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(want_email))
            {
                return Some((access.to_string(), acct_id));
            }
        } else if label == "default" {
            return Some((access.to_string(), acct_id));
        }
    }
    None
}

fn jwt_email(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let val: Value = serde_json::from_slice(&bytes).ok()?;
    val.get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            val.get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}
