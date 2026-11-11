use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::appserver;
use crate::setup;
use crate::usage;

const COOL_AUTH: u64 = 5 * 60;
const COOL_USAGE: u64 = 60 * 60;
const SOLE_CAP: u64 = 60;
const MAX_RESET: u64 = 8 * 24 * 3600;
const USAGE_TTL: u64 = 5 * 60;
const USAGE_DEADLINE: Duration = Duration::from_secs(8);
const HTTP_VARIANTS: [&str; 4] = [
    "httpConnectionFailed",
    "responseStreamConnectionFailed",
    "responseStreamDisconnected",
    "responseTooManyFailedAttempts",
];

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub label: String,
    pub home: PathBuf,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub added_at: u64,
    #[serde(default)]
    pub cooldown_until: u64,
    #[serde(default)]
    pub cooldown_kind: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub last_used_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(default)]
    selected: Option<String>,
    #[serde(default)]
    strategy: Strategy,
}

#[derive(Clone, Debug)]
pub struct Pick {
    pub label: String,
    pub home: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Health {
    Ready,
    Cooling { until: u64, reason: String },
    AuthFailed { until: u64, msg: String },
}

pub enum Serving {
    Account(Pick),
    Ambient,
    AllCooling {
        count: usize,
        label: String,
        until: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Auth,
    Usage,
}

pub enum Outcome {
    Served,
    Failed { fault: Fault, msg: String },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    #[default]
    FillFirst,
    LeastUsed,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Strategy::FillFirst => "fill_first",
            Strategy::LeastUsed => "least_used",
        }
    }
}

impl Health {
    fn until(&self) -> u64 {
        match self {
            Health::Ready => 0,
            Health::Cooling { until, .. } | Health::AuthFailed { until, .. } => *until,
        }
    }

    fn reason(&self) -> &str {
        match self {
            Health::Ready => "",
            Health::Cooling { reason, .. } => reason,
            Health::AuthFailed { msg, .. } => msg,
        }
    }

    fn state(&self) -> &'static str {
        match self {
            Health::Ready => "ready",
            Health::Cooling { .. } => "cooling",
            Health::AuthFailed { .. } => "auth_failed",
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if s == "~" {
        if let Some(h) = home {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    p.to_path_buf()
}

fn store_dir() -> Option<PathBuf> {
    gray_core::paths::gray_home().map(|h| h.join("codex-sub"))
}

fn file_path() -> Option<PathBuf> {
    store_dir().map(|d| d.join("accounts.json"))
}

fn homes_dir() -> Option<PathBuf> {
    store_dir().map(|d| d.join("homes"))
}

fn private_open() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

fn lock_for(path: &Path) -> Result<File, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lock = path.with_file_name(format!("{name}.lock"));
    let file = private_open()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .map_err(|e| format!("lock {}: {e}", lock.display()))?;
    file.lock()
        .map_err(|e| format!("lock {}: {e}", lock.display()))?;
    Ok(file)
}

fn write_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        now_nanos()
    ));
    let res = (|| -> std::io::Result<()> {
        let mut f = private_open().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(|e| format!("{}: {e}", path.display()))
}

fn read_store(path: &Path) -> Result<Store, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Store::default()),
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn load() -> Result<Store, String> {
    match file_path() {
        Some(p) => read_store(&p),
        None => Ok(Store::default()),
    }
}

fn mutate<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let path = file_path().ok_or("cannot resolve GRAY_HOME")?;
    let _lock = lock_for(&path)?;
    let mut store = read_store(&path)?;
    let before = serde_json::to_value(&store).map_err(|e| e.to_string())?;
    let out = f(&mut store)?;
    if serde_json::to_value(&store).map_err(|e| e.to_string())? != before {
        let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
        write_replace(&path, text.as_bytes())?;
    }
    Ok(out)
}

fn health_of(a: &Account, now: u64) -> Health {
    if a.cooldown_until <= now {
        return Health::Ready;
    }
    let reason = a.last_error.clone().unwrap_or_default();
    match a.cooldown_kind.as_deref() {
        Some("auth") => Health::AuthFailed {
            until: a.cooldown_until,
            msg: reason,
        },
        _ => Health::Cooling {
            until: a.cooldown_until,
            reason,
        },
    }
}

fn store_health(a: &mut Account, h: &Health) {
    a.cooldown_until = h.until();
    a.cooldown_kind = match h {
        Health::AuthFailed { .. } => Some("auth".to_string()),
        _ => None,
    };
    a.last_error = match h {
        Health::Ready => None,
        other => Some(other.reason().to_string()),
    };
}

pub fn transition(h: Health, o: &Outcome, now: u64, sole: bool) -> Health {
    let Outcome::Failed { fault, msg } = o else {
        return h;
    };
    let ttl = match fault {
        Fault::Auth => COOL_AUTH,
        Fault::Usage => COOL_USAGE,
    };
    let ttl = if sole {
        ttl.min(SOLE_CAP)
    } else {
        ttl.min(MAX_RESET)
    };
    let until = now + ttl.max(1);
    if h.until() > until {
        return h;
    }
    let reason: String = msg.chars().take(200).collect();
    match fault {
        Fault::Auth => Health::AuthFailed { until, msg: reason },
        Fault::Usage => Health::Cooling { until, reason },
    }
}

pub fn classify(info: &Value) -> Option<Fault> {
    if let Some(code) = info.as_str() {
        return match code {
            "usageLimitExceeded" => Some(Fault::Usage),
            "unauthorized" => Some(Fault::Auth),
            _ => None,
        };
    }
    let obj = info.as_object()?;
    let (name, body) = obj.iter().next()?;
    if obj.len() != 1 || !HTTP_VARIANTS.contains(&name.as_str()) {
        return None;
    }
    match body.get("httpStatusCode").and_then(Value::as_u64) {
        Some(401) => Some(Fault::Auth),
        Some(429) => Some(Fault::Usage),
        _ => None,
    }
}

fn pick_of(a: &Account) -> Pick {
    Pick {
        label: a.label.clone(),
        home: expand_home(&a.home),
    }
}

fn cooling_message(count: usize, label: &str, until: u64) -> String {
    format!(
        "all {count} codex accounts are cooling down; {label} next at {}",
        usage::iso(until)
    )
}

fn serving_of(store: &Store, now: u64) -> Serving {
    if store.accounts.is_empty() {
        return Serving::Ambient;
    }
    if let Some(sel) = store.selected.as_deref()
        && let Some(a) = store
            .accounts
            .iter()
            .find(|a| a.label == sel && health_of(a, now) == Health::Ready)
    {
        return Serving::Account(pick_of(a));
    }
    let next = if store.strategy == Strategy::LeastUsed {
        store
            .accounts
            .iter()
            .filter(|a| health_of(a, now) == Health::Ready)
            .min_by_key(|a| a.last_used_at)
    } else {
        store
            .accounts
            .iter()
            .find(|a| health_of(a, now) == Health::Ready)
    };
    if let Some(a) = next {
        return Serving::Account(pick_of(a));
    }
    let soonest = store
        .accounts
        .iter()
        .map(|a| (a.label.clone(), health_of(a, now)))
        .min_by_key(|(_, h)| h.until());
    match soonest {
        Some((label, h)) => Serving::AllCooling {
            count: store.accounts.len(),
            label,
            until: h.until(),
        },
        None => Serving::Ambient,
    }
}

pub fn pick() -> Result<Option<Pick>, String> {
    let store = load()?;
    match serving_of(&store, now_secs()) {
        Serving::Account(p) => Ok(Some(p)),
        Serving::Ambient => Ok(None),
        Serving::AllCooling {
            count,
            label,
            until,
        } => Err(cooling_message(count, &label, until)),
    }
}

pub fn book(label: &str) {
    let _ = mutate(|store| {
        if let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) {
            a.request_count += 1;
            a.last_used_at = now_secs();
        }
        Ok(())
    });
}

pub fn picks() -> Vec<Pick> {
    load()
        .map(|s| s.accounts.iter().map(pick_of).collect())
        .unwrap_or_default()
}

pub fn settle(label: &str, outcome: Outcome) {
    let _ = mutate(|store| {
        let sole = store.accounts.len() == 1;
        let now = now_secs();
        let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) else {
            return Ok(());
        };
        let h = transition(health_of(a, now), &outcome, now, sole);
        store_health(a, &h);
        Ok(())
    });
}

pub fn resolve(label: &str) -> Option<Pick> {
    if label.is_empty() {
        return None;
    }
    load()
        .ok()?
        .accounts
        .iter()
        .find(|a| a.label == label)
        .map(pick_of)
}

pub fn settle_turn<T>(account: Option<&Pick>, fault: Option<Fault>, r: &Result<T, String>) {
    let Some(p) = account else { return };
    match r {
        Ok(_) => settle(&p.label, Outcome::Served),
        Err(e) => {
            if let Some(fault) = fault {
                settle(
                    &p.label,
                    Outcome::Failed {
                        fault,
                        msg: e.clone(),
                    },
                );
            }
        }
    }
}

pub fn probe_pick() -> Result<Option<Pick>, String> {
    let store = load()?;
    Ok(match serving_of(&store, now_secs()) {
        Serving::Account(p) => Some(p),
        Serving::Ambient => None,
        Serving::AllCooling { .. } => store.accounts.first().map(pick_of),
    })
}

pub fn probe_home() -> Result<PathBuf, String> {
    Ok(probe_pick()?
        .map(|p| p.home)
        .unwrap_or_else(setup::codex_home))
}

fn usage_cache_path(label: Option<&str>) -> Option<PathBuf> {
    let dir = store_dir()?;
    Some(match label {
        Some(l) => dir.join(format!("usage-{l}.json")),
        None => dir.join("usage.json"),
    })
}

fn read_cache(path: &Path) -> Option<(u64, Value)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((v.get("fetched_at")?.as_u64()?, v.get("limits")?.clone()))
}

fn drop_cache(path: &Path) {
    if let Ok(_lock) = lock_for(path) {
        let _ = std::fs::remove_file(path);
    }
}

fn write_cache(path: &Path, card: &Value) {
    let text = json!({"fetched_at": now_secs(), "limits": card}).to_string();
    if let Ok(_lock) = lock_for(path) {
        let _ = write_replace(path, text.as_bytes());
    }
}

const USAGE: &str = "\
codex-sub account — multiple Codex logins behind one provider.
  codex-sub account add [--home <dir>] <label>   run `codex login` into a new
                                                  CODEX_HOME (or adopt an existing one)
  codex-sub account list                          accounts + health
  codex-sub account use <label>                   pin an account (use --clear to unpin)
  codex-sub account remove [--keep] <label>       drop an account (+its managed home)
  codex-sub account strategy <fill_first|least_used>
                                                  pick order among usable accounts
  codex-sub account status                        which account serves now
  codex-sub account usage [<label>]               quota windows per account, or one
  any verb accepts --json for the machine contract the host /accounts panel reads
The file is <gray home>/codex-sub/accounts.json; absent or empty
`accounts` keeps the single ambient `codex` login.";

pub fn command(args: &[String], from_argv: bool) -> Result<String, String> {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--json")
        .cloned()
        .collect();
    match args.first().map(String::as_str) {
        Some("add") => cmd_add(&args[1..], json, from_argv),
        Some("use") => cmd_use(&args[1..], json),
        Some("remove") | Some("rm") => cmd_remove(&args[1..], json),
        Some("strategy") => cmd_strategy(&args[1..], json),
        Some("status") => cmd_status(json),
        Some("usage") => cmd_usage(&args[1..], json),
        Some("list") | None => cmd_list(json),
        Some(other) => Err(format!("unknown account verb {other:?}\n\n{USAGE}")),
    }
}

fn store_json(store: &Store) -> Result<String, String> {
    let now = now_secs();
    let accounts: Vec<Value> = store
        .accounts
        .iter()
        .map(|a| {
            let h = health_of(a, now);
            json!({
                "label": a.label,
                "home": a.home.display().to_string(),
                "email": a.email,
                "added_at": a.added_at,
                "cooldown_until": a.cooldown_until,
                "cooling": !matches!(h, Health::Ready),
                "health": {"state": h.state(), "until": h.until(), "reason": h.reason()},
                "last_error": a.last_error,
                "request_count": a.request_count,
                "last_used_at": a.last_used_at,
                "pinned": store.selected.as_deref() == Some(a.label.as_str()),
            })
        })
        .collect();
    let serving = match serving_of(store, now) {
        Serving::Account(p) => json!({"kind": "account", "label": p.label}),
        Serving::Ambient => json!({"kind": "ambient"}),
        Serving::AllCooling {
            count,
            label,
            until,
        } => json!({
            "kind": "cooldown",
            "until": until,
            "reason": cooling_message(count, &label, until),
        }),
    };
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "plugin": crate::manifest::PLUGIN_NAME,
        "provider": crate::manifest::PROVIDER_ID,
        "strategy": store.strategy.name(),
        "selected": store.selected,
        "ambient": store.accounts.is_empty(),
        "accounts": accounts,
        "serving": serving,
    }))
    .map_err(|e| e.to_string())
}

fn ok_json(extra: Value) -> Result<String, String> {
    let mut v = extra;
    v["ok"] = json!(true);
    serde_json::to_string(&v).map_err(|e| e.to_string())
}

fn valid_label(label: &str) -> Result<(), String> {
    let ok = !label.is_empty()
        && label.len() <= 40
        && label
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(())
        .ok_or_else(|| format!("bad label {label:?}: 1-40 chars of [A-Za-z0-9_-]"))
}

fn discard(stage: &Option<PathBuf>, e: String) -> String {
    if let Some(s) = stage {
        let _ = std::fs::remove_dir_all(s);
    }
    e
}

fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn cmd_add(args: &[String], json: bool, from_argv: bool) -> Result<String, String> {
    let mut home: Option<PathBuf> = None;
    let mut label: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--home" {
            let Some(h) = it.next() else {
                return Err("--home needs a directory".into());
            };
            home = Some(expand_home(Path::new(h)));
        } else if a.starts_with('-') {
            return Err(format!("unknown flag {a:?}\n\n{USAGE}"));
        } else if label.is_none() {
            label = Some(a.clone());
        } else {
            return Err(USAGE.into());
        }
    }
    let label = label.ok_or_else(|| format!("account add needs a label\n\n{USAGE}"))?;
    valid_label(&label)?;
    if load()?.accounts.iter().any(|a| a.label == label) {
        return Err(format!("account {label:?} already exists"));
    }
    let bin = setup::resolve_command().ok_or(setup::INSTALL_HINT)?;
    let managed = home.is_none();
    if managed && (!from_argv || !std::io::IsTerminal::is_terminal(&std::io::stdin())) {
        return Err(
            "account add runs `codex login`; use `gray codex-sub account add <label>` in a terminal"
                .into(),
        );
    }
    let (home, stage) = match home {
        Some(h) => {
            if !h.is_dir() {
                return Err(format!("no directory {}", h.display()));
            }
            let h = h
                .canonicalize()
                .map_err(|e| format!("{}: {e}", h.display()))?;
            (h, None)
        }
        None => {
            let dir = homes_dir().ok_or("cannot resolve GRAY_HOME")?;
            let home = dir.join(&label);
            if home.exists() {
                return Err(format!("credential home {} already exists", home.display()));
            }
            let stage = dir.join(format!(
                ".{label}.{}.{}.tmp",
                std::process::id(),
                now_nanos()
            ));
            private_dir(&stage)?;
            (home, Some(stage))
        }
    };
    let login_home = stage.clone().unwrap_or_else(|| home.clone());
    if managed {
        eprintln!("logging {label:?} into {} …", home.display());
        let status = std::process::Command::new(&bin)
            .arg("login")
            .envs(setup::child_env(&login_home))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| discard(&stage, format!("spawn codex login: {e}")))?;
        if !status.success() {
            return Err(discard(&stage, format!("`codex login` exited {status}")));
        }
    }
    let email = match appserver::probe_at(&login_home) {
        appserver::Probe::Ready(v) => v
            .pointer("/account/email")
            .and_then(Value::as_str)
            .map(str::to_string),
        appserver::Probe::NotInstalled => {
            return Err(discard(&stage, setup::INSTALL_HINT.into()));
        }
        appserver::Probe::Failed(e) => {
            return Err(discard(
                &stage,
                format!("account/read in {}: {e}", login_home.display()),
            ));
        }
        appserver::Probe::NotSignedIn(_) => {
            let why = if managed {
                "`codex login` did not complete"
            } else {
                "adopt a directory that holds a real login"
            };
            return Err(discard(
                &stage,
                format!("no login in {} — {why}", home.display()),
            ));
        }
    };
    let placed = match &stage {
        Some(s) => {
            std::fs::rename(s, &home)
                .map_err(|e| discard(&stage, format!("place {}: {e}", home.display())))?;
            Some(home.clone())
        }
        None => None,
    };
    let inserted = mutate(|store| {
        if store.accounts.iter().any(|a| a.label == label) {
            return Err(format!("account {label:?} already exists"));
        }
        if let Some(e) = &email
            && let Some(dup) = store
                .accounts
                .iter()
                .find(|a| a.email.as_deref() == Some(e.as_str()))
        {
            return Err(format!(
                "that login is already registered as {:?} ({e})",
                dup.label
            ));
        }
        store.accounts.push(Account {
            label: label.clone(),
            home: home.clone(),
            email: email.clone(),
            added_at: now_secs(),
            cooldown_until: 0,
            cooldown_kind: None,
            last_error: None,
            request_count: 0,
            last_used_at: 0,
        });
        if store.selected.is_none() {
            store.selected = Some(label.clone());
        }
        Ok(store.selected.clone())
    });
    let selected = match inserted {
        Ok(s) => s,
        Err(e) => {
            if let Some(p) = &placed {
                let _ = std::fs::remove_dir_all(p);
            }
            return Err(e);
        }
    };
    let who = email.unwrap_or_else(|| "identity pending".to_string());
    if json {
        return ok_json(json!({
            "message": format!("account {label:?} added"),
            "label": label,
            "email": who,
            "selected": selected,
        }));
    }
    Ok(format!(
        "account {label:?} added ({who}); home {}",
        home.display()
    ))
}

fn cmd_use(args: &[String], json: bool) -> Result<String, String> {
    match args.first().map(String::as_str) {
        None => {
            let selected = load()?.selected;
            if json {
                return ok_json(json!({"selected": selected}));
            }
            Ok(format!(
                "pinned account: {}",
                selected.as_deref().unwrap_or("(none)")
            ))
        }
        Some("--clear") | Some("none") | Some("auto") => {
            let selected = mutate(|store| {
                store.selected = None;
                Ok(store.selected.clone())
            })?;
            if json {
                return ok_json(json!({"selected": selected}));
            }
            Ok("pin cleared — selection follows the strategy".to_string())
        }
        Some(label) => {
            let want = label.to_string();
            let selected = mutate(|store| {
                if !store.accounts.iter().any(|a| a.label == want) {
                    return Err(format!("no account {want:?}"));
                }
                store.selected = Some(want.clone());
                Ok(store.selected.clone())
            })?;
            if json {
                return ok_json(json!({"selected": selected}));
            }
            Ok(format!("serving account: {label}"))
        }
    }
}

fn cmd_strategy(args: &[String], json: bool) -> Result<String, String> {
    match args.first().map(String::as_str) {
        None => {
            let store = load()?;
            let cur = store.strategy.name().to_string();
            if json {
                return ok_json(json!({"strategy": cur}));
            }
            Ok(format!("strategy: {cur}"))
        }
        Some(name @ ("fill_first" | "least_used")) => {
            let next = if name == "least_used" {
                Strategy::LeastUsed
            } else {
                Strategy::FillFirst
            };
            let strategy = mutate(|store| {
                store.strategy = next;
                Ok(store.strategy.name().to_string())
            })?;
            if json {
                return ok_json(json!({"strategy": strategy}));
            }
            Ok(format!("strategy: {strategy}"))
        }
        Some(other) => Err(format!(
            "unknown strategy {other:?} — fill_first or least_used"
        )),
    }
}

fn cmd_remove(args: &[String], json: bool) -> Result<String, String> {
    let mut keep = false;
    let mut label: Option<String> = None;
    for a in args {
        if a == "--keep" {
            keep = true;
        } else if label.is_none() {
            label = Some(a.clone());
        } else {
            return Err(USAGE.into());
        }
    }
    let label = label.ok_or_else(|| format!("account remove needs a label\n\n{USAGE}"))?;
    let home = load()?
        .accounts
        .iter()
        .find(|a| a.label == label)
        .map(|a| expand_home(&a.home))
        .ok_or_else(|| format!("no account {label:?}"))?;
    crate::session::shutdown_account(&label);
    let mut msg = format!("account {label:?} removed");
    if !keep
        && let Some(dir) = homes_dir()
        && home.starts_with(&dir)
        && home.is_dir()
    {
        let trash = dir.join(format!(
            ".{label}.{}.{}.trash",
            std::process::id(),
            now_nanos()
        ));
        std::fs::rename(&home, &trash)
            .map_err(|e| format!("cannot remove {}: {e}", home.display()))?;
        std::fs::remove_dir_all(&trash)
            .map_err(|e| format!("cannot remove {}: {e}", trash.display()))?;
        msg.push_str(&format!(" and its credential home {}", home.display()));
    }
    mutate(|store| {
        let i = store
            .accounts
            .iter()
            .position(|a| a.label == label)
            .ok_or_else(|| format!("no account {label:?}"))?;
        store.accounts.remove(i);
        if store.selected.as_deref() == Some(label.as_str()) {
            store.selected = None;
        }
        Ok(())
    })?;
    if let Some(p) = usage_cache_path(Some(&label)) {
        drop_cache(&p);
    }
    if json {
        return ok_json(json!({"message": msg}));
    }
    Ok(msg)
}

fn account_line(a: &Account, now: u64, selected: &Option<String>) -> String {
    let mark = if selected.as_deref() == Some(a.label.as_str()) {
        '*'
    } else {
        ' '
    };
    let state = match health_of(a, now) {
        Health::Ready => "ok".to_string(),
        Health::Cooling { until, .. } => format!("cooled until {}", usage::iso(until)),
        Health::AuthFailed { until, .. } => format!("auth failed until {}", usage::iso(until)),
    };
    let who = a.email.clone().unwrap_or_else(|| "-".to_string());
    format!(
        "{mark} {:<16} {:<12} {:<30} {}",
        a.label,
        state,
        who,
        a.home.display()
    )
}

fn cmd_list(json: bool) -> Result<String, String> {
    let store = load()?;
    if json {
        return store_json(&store);
    }
    if store.accounts.is_empty() {
        return Ok("no accounts configured — turns use the ambient `codex` login".to_string());
    }
    let now = now_secs();
    let mut out = format!("{} account(s):\n", store.accounts.len());
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        out.push('\n');
    }
    out.push_str("`use` pins *; fill_first order is file order.");
    Ok(out)
}

fn cmd_status(json: bool) -> Result<String, String> {
    let store = load()?;
    if json {
        return store_json(&store);
    }
    if store.accounts.is_empty() {
        return Ok("no accounts configured — serving the ambient `codex` login".to_string());
    }
    let now = now_secs();
    let mut out = match serving_of(&store, now) {
        Serving::Account(p) => {
            format!("serving account: {} (home {})\n", p.label, p.home.display())
        }
        Serving::Ambient => "serving account: (none)\n".to_string(),
        Serving::AllCooling {
            count,
            label,
            until,
        } => format!(
            "serving account: {}\n",
            cooling_message(count, &label, until)
        ),
    };
    out.push_str(&format!("strategy: {}\n", store.strategy.name()));
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        if let Some(e) = &a.last_error {
            out.push_str(&format!("  last error: {e}"));
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

fn usage_entry(label: Option<String>, outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(card) => json!({"label": label, "usage": card, "error": null}),
        Err(e) => json!({"label": label, "usage": null, "error": e}),
    }
}

fn usage_targets(
    store: &Store,
    want: Option<&str>,
) -> Vec<(Option<String>, Result<PathBuf, String>)> {
    if store.accounts.is_empty() {
        return match want {
            None => vec![(None, Ok(setup::codex_home()))],
            Some(w) => vec![(
                Some(w.to_string()),
                Err(format!(
                    "no account labelled {w:?}: no pool configured, the ambient login has no label"
                )),
            )],
        };
    }
    let picked: Vec<(Option<String>, Result<PathBuf, String>)> = store
        .accounts
        .iter()
        .filter(|a| want.is_none_or(|w| a.label == w))
        .map(|a| (Some(a.label.clone()), Ok(expand_home(&a.home))))
        .collect();
    match want {
        Some(w) if picked.is_empty() => vec![(
            Some(w.to_string()),
            Err(format!("no account labelled {w:?}")),
        )],
        _ => picked,
    }
}

fn usage_one(label: Option<String>, home: PathBuf) -> Value {
    let cache = usage_cache_path(label.as_deref());
    let cached = cache.as_deref().and_then(read_cache);
    if let Some((at, card)) = &cached
        && at + USAGE_TTL > now_secs()
    {
        return usage_entry(label, Ok(card.clone()));
    }
    match usage::card_at(&home) {
        Ok(card) => {
            if let Some(p) = &cache {
                write_cache(p, &card);
            }
            usage_entry(label, Ok(card))
        }
        Err(e) => match cached {
            Some((_, card)) => json!({"label": label, "usage": card, "error": e}),
            None => usage_entry(label, Err(e)),
        },
    }
}

fn usage_text(entry: &Value) -> String {
    let mut out = match entry.get("label").and_then(Value::as_str) {
        Some(l) => format!("{l}\n"),
        None => "ambient\n".to_string(),
    };
    if let Some(e) = entry.get("error").and_then(Value::as_str) {
        out.push_str(&format!("  error: {e}\n"));
    }
    let windows = entry.pointer("/usage/windows").and_then(Value::as_array);
    if entry.get("usage").is_some_and(|u| !u.is_null()) && windows.is_none_or(Vec::is_empty) {
        out.push_str("  no quota windows reported\n");
    }
    for w in windows.into_iter().flatten() {
        let name = w
            .get("label")
            .or(w.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let pct = w
            .get("used_percent")
            .and_then(Value::as_f64)
            .map_or("-".to_string(), |p| format!("{p:.0}%"));
        let reset = w.get("resets_at").and_then(Value::as_str).unwrap_or("-");
        out.push_str(&format!("  {name:<14} {pct:>5}  resets {reset}\n"));
    }
    out.trim_end().to_string()
}

fn cmd_usage(args: &[String], json: bool) -> Result<String, String> {
    let want = args.first().map(String::as_str);
    let targets = usage_targets(&load()?, want);
    let labels: Vec<Option<String>> = targets.iter().map(|(l, _)| l.clone()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut done: Vec<Option<Value>> = vec![None; targets.len()];
    for (i, (label, home)) in targets.into_iter().enumerate() {
        match home {
            Err(e) => done[i] = Some(usage_entry(label, Err(e))),
            Ok(home) => {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send((i, usage_one(label, home)));
                });
            }
        }
    }
    drop(tx);
    let deadline = Instant::now() + USAGE_DEADLINE;
    while let Ok((i, entry)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        done[i] = Some(entry);
    }
    let entries: Vec<Value> = done
        .into_iter()
        .zip(labels)
        .map(|(d, label)| {
            d.unwrap_or_else(|| {
                usage_entry(
                    label,
                    Err(format!("no reply within {}s", USAGE_DEADLINE.as_secs())),
                )
            })
        })
        .collect();
    if json {
        return serde_json::to_string_pretty(&json!({
            "version": 1,
            "plugin": crate::manifest::PLUGIN_NAME,
            "provider": crate::manifest::PROVIDER_ID,
            "accounts": entries,
        }))
        .map_err(|e| e.to_string());
    }
    Ok(entries
        .iter()
        .map(usage_text)
        .collect::<Vec<_>>()
        .join("\n"))
}
