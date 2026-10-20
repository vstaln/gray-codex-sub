//! Multi-account support: N Codex credential homes behind one provider.
//!
//! One account = one `CODEX_HOME` the Codex CLI signed into (`codex
//! login` writes `auth.json` there; the sidecar never reads it). State
//! lives in `<gray home>/codex-sub/accounts.json` next to `tools.json`:
//!
//! ```json
//! {"strategy": "fill_first", "preferred": "work",
//!  "accounts": [{"label": "work", "home": "/abs/path", "added_at": 0}],
//!  "health": {"<home>": {"status": "ok|cooled", "cooled_until": 0,
//!              "reset_at": 0, "last_error": "", "request_count": 0,
//!              "last_used_at": 0}}}
//! ```
//!
//! Zero-config back-compat: no `accounts` (or an inherited `CODEX_HOME`
//! pin) means one implicit account — the home `setup::codex_home()`
//! already resolves. An env `CODEX_HOME` pin is an operator override:
//! the pool collapses to that sole home, configured accounts or not.
//!
//! Selection is `fill_first` (priority order, first non-cooled account)
//! with an optional `preferred` pin set by `account use`; `round_robin`
//! (least-recently-served) and `least_used` (fewest requests) are the
//! alternates. Cooldowns follow 9router's rule: only account-scoped
//! failures bench — auth (401/login) and 429/quota — and a declared
//! reset (Retry-After) beats the computed TTL. Request-scoped errors
//! (bad request, dead child, model complaints) never cool an account,
//! and a sole account's bench is capped so one account can't hide the
//! real error behind "all accounts locked".

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::setup;

/// Auth-shaped failures bench the account briefly: a dead login won't
/// heal inside a minute, but shouldn't lock the slot for long either.
const AUTH_TTL_SECS: u64 = 300;
/// 429/quota: Codex windows are 5h/weekly — bench up to an hour unless
/// the upstream message declares a reset, which wins uncapped.
const RATE_TTL_SECS: u64 = 3600;
/// A sole account never benches past this: with nothing to fail over
/// to, a long lock just masks the real upstream error.
const SOLE_CAP_SECS: u64 = 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One credential home the pool may serve from.
#[derive(Clone)]
pub struct Account {
    pub label: String,
    pub home: PathBuf,
    /// The zero-config sole account (`setup::codex_home()`), not a
    /// stored entry — shown as `(implicit)` in listings.
    pub implicit: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Health {
    #[serde(default)]
    status: String,
    #[serde(default)]
    cooled_until: u64,
    /// Upstream-declared reset, when the error text carried one.
    #[serde(default)]
    reset_at: u64,
    #[serde(default)]
    last_error: String,
    #[serde(default)]
    request_count: u64,
    #[serde(default)]
    last_used_at: u64,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    label: String,
    home: String,
    #[serde(default)]
    added_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// `fill_first` (default) | `round_robin` | `least_used`.
    #[serde(default)]
    strategy: String,
    /// `account use <label>` pin — wins selection whenever available.
    #[serde(default)]
    preferred: Option<String>,
    #[serde(default)]
    accounts: Vec<Entry>,
    /// Per-home health keyed by the stored home path string, so a label
    /// edit can't shed a cooldown and a remove can't leak one.
    #[serde(default)]
    health: BTreeMap<String, Health>,
}

/// `<gray home>/codex-sub/accounts.json` — the same GRAY_HOME rule
/// `tools.json` already uses.
fn file_path() -> Option<PathBuf> {
    gray_core::paths::gray_home().map(|h| h.join("codex-sub").join("accounts.json"))
}

/// Corrupt or missing state reads as empty: a turn must never fail on a
/// pool file, same fail-closed rule as the tool policy.
fn load() -> State {
    let Some(path) = file_path() else {
        return State::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return State::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save(state: &State) -> Result<(), String> {
    let path = file_path().ok_or("cannot resolve GRAY_HOME")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("{e}"))?;
        use std::io::Write;
        f.write_all(
            serde_json::to_string_pretty(state)
                .unwrap_or_else(|_| "{}".to_string())
                .as_bytes(),
        )
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("{e}"))
}

/// An env `CODEX_HOME` pins the credential home — an operator override
/// the pool honors by collapsing to that sole account, so a host that
/// exports it sees exactly today's single-home behavior.
pub fn env_pinned() -> bool {
    std::env::var_os("CODEX_HOME").is_some_and(|v| !v.is_empty())
}

/// The zero-config account: label `default`, the resolved Codex home.
fn implicit() -> Account {
    Account {
        label: "default".to_string(),
        home: setup::codex_home(),
        implicit: true,
    }
}

/// Accounts in selection (priority) order. Explicit entries win; the
/// implicit sole account covers the unconfigured and env-pinned cases.
fn pool_in(state: &State) -> Vec<Account> {
    if env_pinned() || state.accounts.is_empty() {
        return vec![implicit()];
    }
    state
        .accounts
        .iter()
        .map(|e| Account {
            label: e.label.clone(),
            home: PathBuf::from(&e.home),
            implicit: false,
        })
        .collect()
}

/// The account pool as it stands right now.
pub fn pool() -> Vec<Account> {
    pool_in(&load())
}

/// Every configured account home — used to scope session rewarming so a
/// record never re-pools under a home that left the pool.
pub fn pool_homes() -> Vec<PathBuf> {
    pool().iter().map(|a| a.home.clone()).collect()
}

fn home_key(home: &Path) -> String {
    home.to_string_lossy().into_owned()
}

fn cooled(state: &State, home: &Path, now: u64) -> bool {
    state
        .health
        .get(&home_key(home))
        .is_some_and(|h| h.cooled_until > now)
}

/// Pick the account a turn should try first, honoring `excluded`
/// (labels already benched this request), active cooldowns, the
/// `preferred` pin, then the configured strategy.
pub fn pick(excluded: &BTreeSet<String>) -> Result<Account, String> {
    let state = load();
    let now = now_secs();
    let pool = pool_in(&state);
    let avail: Vec<Account> = pool
        .iter()
        .filter(|a| !excluded.contains(&a.label) && !cooled(&state, &a.home, now))
        .cloned()
        .collect();
    if avail.is_empty() {
        // Surface the real blocker, not a bare "all locked": earliest
        // retry plus the error that benched the pool.
        let soonest = pool
            .iter()
            .filter_map(|a| state.health.get(&home_key(&a.home)).map(|h| (a, h)))
            .min_by_key(|(_, h)| h.cooled_until);
        if let Some((a, h)) = soonest {
            let wait = h.cooled_until.saturating_sub(now);
            let why = if h.last_error.is_empty() {
                String::new()
            } else {
                format!(" — last error: {}", h.last_error.chars().take(160).collect::<String>())
            };
            return Err(format!(
                "all codex accounts are cooling; earliest retry for '{}' in {wait}s{why}",
                a.label
            ));
        }
        return Err("no codex account is available".to_string());
    }
    if let Some(want) = state.preferred.as_deref()
        && let Some(a) = avail.iter().find(|a| a.label == want)
    {
        return Ok(a.clone());
    }
    let health_of = |a: &Account| state.health.get(&home_key(&a.home));
    let chosen = match state.strategy.as_str() {
        // Least-recently-served rotates fairly without a cursor: serving
        // stamps last_used_at, so the oldest idle account is next.
        "round_robin" => avail
            .iter()
            .min_by_key(|a| health_of(a).map(|h| h.last_used_at).unwrap_or(0)),
        "least_used" => avail
            .iter()
            .min_by_key(|a| health_of(a).map(|h| h.request_count).unwrap_or(0)),
        _ => avail.first(),
    };
    Ok(chosen.cloned().unwrap_or_else(|| avail[0].clone()))
}

/// The account probes (`account/read`, `model/list`, rate limits) should
/// speak for: what a turn would serve right now, or the pool head when
/// everything is benched — the status panel stays truthful either way.
pub fn serving() -> Account {
    pick(&BTreeSet::new()).unwrap_or_else(|_| pool().into_iter().next().unwrap_or_else(implicit))
}

/// The home probes should spawn under.
pub fn serving_home() -> PathBuf {
    serving().home
}

/// Any pool home carrying a CLI login — the fast pre-chat gate.
pub fn any_login() -> bool {
    pool().iter().any(|a| setup::login_present(&a.home))
}

/// Does this error bench the account? Only account-scoped failures
/// count — the 9router scar: a request-scoped 4xx cooling an account
/// hides the real error behind "all accounts locked".
///
/// Returns the absolute unix time the account stays benched, or `None`
/// for errors that must not cool anything.
pub fn failure_bench(err: &str, sole: bool) -> Option<u64> {
    let m = err.to_lowercase();
    let ttl = if m.contains("429")
        || m.contains("rate limit")
        || m.contains("rate_limit")
        || m.contains("ratelimit")
        || m.contains("too many requests")
        || m.contains("usage limit")
        || m.contains("quota")
    {
        RATE_TTL_SECS
    } else if m.contains("401")
        || m.contains("unauthorized")
        || m.contains("not logged in")
        || m.contains("not signed in")
        || m.contains("authentication")
        || m.contains("invalid api key")
        || m.contains("invalid_grant")
    {
        AUTH_TTL_SECS
    } else {
        return None;
    };
    let now = now_secs();
    // A declared reset (Retry-After, resets-at) wins over the computed
    // TTL — the provider knows its own window.
    let until = retry_after(&m, now).unwrap_or(now + ttl);
    Some(if sole { until.min(now + SOLE_CAP_SECS) } else { until })
}

/// Pull an upstream-declared reset out of an error message: explicit
/// `retry after N`, or a bare future unix timestamp (rate-limit windows
/// are reported as epoch seconds). Returns absolute unix seconds.
fn retry_after(m: &str, now: u64) -> Option<u64> {
    for needle in ["retry after", "retry-after", "retry_after", "try again in"] {
        if let Some(pos) = m.find(needle) {
            let tail = &m[pos + needle.len()..];
            let digits: String = tail
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u64>()
                && n > 0
            {
                return Some(now + n.min(86_400));
            }
        }
    }
    // A standalone epoch is the declared reset — 10 digits for seconds,
    // 13 for millis. Tight bounds keep ids/ports from matching.
    let mut digits = String::new();
    let mut best = None;
    for c in m.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        if (10..=13).contains(&digits.len())
            && let Ok(n) = digits.parse::<u64>()
        {
            let secs = if digits.len() > 11 { n / 1000 } else { n };
            if secs > now && secs < now + 90 * 86_400 {
                best = Some(best.map_or(secs, |b: u64| b.min(secs)));
            }
        }
        digits.clear();
    }
    best
}

/// Bench an account after an auth/429 failure. Best-effort: a state
/// write that fails just means the next request relearns it.
pub fn mark_cooled(account: &Account, err: &str, until: u64) {
    let mut state = load();
    let h = state.health.entry(home_key(&account.home)).or_default();
    h.status = "cooled".to_string();
    h.cooled_until = until;
    h.reset_at = retry_after(&err.to_lowercase(), now_secs()).unwrap_or(0);
    h.last_error = err.chars().take(300).collect();
    let _ = save(&state);
}

/// Record a served turn: request count + last-used stamp, and clear any
/// stale cooldown — a success proves the account healthy.
pub fn note_used(account: &Account) {
    let mut state = load();
    let h = state.health.entry(home_key(&account.home)).or_default();
    h.status = "ok".to_string();
    h.cooled_until = 0;
    h.reset_at = 0;
    h.last_error.clear();
    h.request_count = h.request_count.saturating_add(1);
    h.last_used_at = now_secs();
    let _ = save(&state);
}

/// Labels name pool entries and default home paths — keep them filename
/// and table safe.
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 40
        && label != "default"
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `~/.gray/codex-sub/accounts/<label>/` — the managed per-account home.
fn default_home(label: &str) -> Result<PathBuf, String> {
    gray_core::paths::gray_home()
        .map(|h| h.join("codex-sub").join("accounts").join(label))
        .ok_or_else(|| "cannot resolve GRAY_HOME".to_string())
}

const ACCOUNT_USAGE: &str = "\
account — Codex credential homes (one `codex login` each).
  account                    list accounts
  account add <label> [--home <path>] [-- <codex login args>]
                             sign in a new account (spawns `codex login`)
  account use <label>        prefer this account when it is healthy
  account use                show the current preference
  account remove <label>     drop an account (its home dir stays on disk)
State: <gray home>/codex-sub/accounts.json — strategy fill_first |
round_robin | least_used.";

fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "never".to_string();
    }
    let d = now.saturating_sub(then);
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

fn list(state: &State) -> String {
    let pool = pool_in(state);
    let now = now_secs();
    let mut out = String::new();
    if env_pinned() {
        out.push_str("note: CODEX_HOME is pinned in env — it overrides the account pool.\n");
    }
    if !state.accounts.is_empty() {
        out.push_str(&format!(
            "strategy: {} · preferred: {}\n",
            if state.strategy.is_empty() {
                "fill_first"
            } else {
                &state.strategy
            },
            state.preferred.as_deref().unwrap_or("(none)")
        ));
    }
    for a in &pool {
        let h = state.health.get(&home_key(&a.home));
        let mark = if state.preferred.as_deref() == Some(a.label.as_str()) {
            "*"
        } else {
            " "
        };
        let login = if setup::login_present(&a.home) {
            "yes"
        } else {
            "no"
        };
        let mut flags: Vec<String> = Vec::new();
        if let Some(h) = h {
            if h.cooled_until > now {
                flags.push(format!("cooled {}s", h.cooled_until - now));
            } else if !h.status.is_empty() {
                flags.push(h.status.clone());
            }
            if h.request_count > 0 {
                flags.push(format!("{} reqs, last {}", h.request_count, ago(now, h.last_used_at)));
            }
            if !h.last_error.is_empty() && h.cooled_until > now {
                flags.push(format!("err: {}", h.last_error.chars().take(80).collect::<String>()));
            }
        }
        let tag = if a.implicit { " (implicit)" } else { "" };
        out.push_str(&format!(
            "{mark} {:<14} {:<48} login:{:<3} {}{tag}\n",
            a.label,
            a.home.display(),
            login,
            flags.join(" · "),
        ));
    }
    out.trim_end().to_string()
}

/// Register a signed-in account — shared by the interactive add path
/// and any caller that already has a logged-in home.
fn register(state: &mut State, label: &str, home: &Path) -> Result<String, String> {
    if state.accounts.iter().any(|e| e.label == label) {
        return Err(format!("account '{label}' already exists"));
    }
    let home_str = home.to_string_lossy().into_owned();
    if let Some(e) = state.accounts.iter().find(|e| e.home == home_str) {
        return Err(format!("home {home_str} is already account '{}'", e.label));
    }
    state.accounts.push(Entry {
        label: label.to_string(),
        home: home_str.clone(),
        added_at: now_secs(),
    });
    save(state)?;
    Ok(home_str)
}

/// Who the new home is signed in as — a best-effort confirmation line
/// after `codex login`, never a failure source.
fn whoami_line(home: &Path) -> String {
    match crate::appserver::probe_at(home) {
        crate::appserver::Probe::Ready(v) => {
            let a = v.get("account").cloned().unwrap_or(Value::Null);
            let email = a.get("email").and_then(Value::as_str).unwrap_or("?");
            let plan = a.get("planType").and_then(Value::as_str).unwrap_or("?");
            format!("\n  signed in as {email} · plan {plan}")
        }
        _ => String::new(),
    }
}

/// `account add`: sign a new credential home in with `codex login`.
/// `allow_login` is the argv path — the sidecar wire can't drive an
/// interactive login, so there `add` only registers a home that already
/// carries a login (or prints the shell command to run).
fn add(args: &[String], allow_login: bool) -> String {
    let Some(label) = args.first() else {
        return format!("account add needs a label.\n\n{ACCOUNT_USAGE}");
    };
    if !valid_label(label) {
        return format!("bad label {label:?} — 1-40 chars, [a-zA-Z0-9_-], not 'default'.");
    }
    let mut home: Option<PathBuf> = None;
    let mut login_args: Vec<String> = Vec::new();
    let mut i = 1;
    let mut rest = false;
    while i < args.len() {
        if rest {
            login_args.push(args[i].clone());
        } else if args[i] == "--" {
            rest = true;
        } else if args[i] == "--home" {
            i += 1;
            match args.get(i) {
                Some(p) => home = Some(PathBuf::from(p)),
                None => return "account add: --home needs a path".to_string(),
            }
        } else {
            return format!("account add: unknown arg {:?}\n\n{ACCOUNT_USAGE}", args[i]);
        }
        i += 1;
    }
    let home = match home {
        Some(h) => h,
        None => match default_home(label) {
            Ok(h) => h,
            Err(e) => return format!("account add: {e}"),
        },
    };
    let mut state = load();
    if state.accounts.iter().any(|e| e.label == *label) {
        return format!("account '{label}' already exists — `account remove {label}` first.");
    }
    if env_pinned() {
        return "CODEX_HOME is pinned in env; unset it to use the account pool.".to_string();
    }
    if !setup::login_present(&home) {
        if !allow_login {
            return format!(
                "no Codex login in {}. From a shell run:\n  gray codex-sub account add {label}",
                home.display()
            );
        }
        let Some(bin) = setup::resolve_command() else {
            return setup::INSTALL_HINT.to_string();
        };
        if let Err(e) = std::fs::create_dir_all(&home) {
            return format!("cannot create {}: {e}", home.display());
        }
        println!("signing in '{label}' — codex login follows in this terminal…");
        let status = std::process::Command::new(&bin)
            .arg("login")
            .args(&login_args)
            .envs(setup::child_env(&home))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        match status {
            Ok(s) if s.success() && setup::login_present(&home) => {}
            Ok(s) if s.success() => {
                return format!(
                    "`codex login` exited cleanly but no login appeared in {} — account '{label}' not added.",
                    home.display()
                );
            }
            Ok(s) => {
                return format!("`codex login` exited {s} — account '{label}' not added.");
            }
            Err(e) => return format!("spawn {bin}: {e}"),
        }
    }
    match register(&mut state, label, &home) {
        Err(e) => e,
        Ok(home_str) => {
            let mut out = format!("added account '{label}' — home {home_str}");
            out.push_str(&whoami_line(&home));
            if state.accounts.len() == 1 {
                out.push_str(&format!(
                    "\n  note: the previous default home {} is NOT in the pool — \
                     re-add it with `--home` to keep it in rotation.",
                    setup::codex_home().display()
                ));
            }
            if state.preferred.is_none() {
                out.push_str("\n  (`account use` pins a preferred account; until then fill_first order applies.)");
            }
            out
        }
    }
}

fn use_account(args: &[String]) -> String {
    let mut state = load();
    match args.first() {
        None => match &state.preferred {
            Some(p) => format!("preferred: {p}"),
            None => "no preferred account — fill_first order applies".to_string(),
        },
        Some(label) => {
            if !pool_in(&state).iter().any(|a| a.label == *label) {
                return format!("unknown account '{label}'");
            }
            state.preferred = Some(label.clone());
            match save(&state) {
                Ok(()) => format!("preferred account: {label}"),
                Err(e) => format!("could not save: {e}"),
            }
        }
    }
}

fn remove(args: &[String]) -> String {
    let Some(label) = args.first() else {
        return format!("account remove needs a label.\n\n{ACCOUNT_USAGE}");
    };
    let mut state = load();
    let Some(pos) = state.accounts.iter().position(|e| e.label == *label) else {
        return format!("unknown account '{label}'");
    };
    let entry = state.accounts.remove(pos);
    state.health.remove(&entry.home);
    if state.preferred.as_deref() == Some(label.as_str()) {
        state.preferred = None;
    }
    match save(&state) {
        Ok(()) => format!(
            "removed account '{label}' (home left on disk: {})",
            entry.home
        ),
        Err(e) => format!("could not save: {e}"),
    }
}

/// The `account` verb for both surfaces. `allow_login` is true only on
/// the direct argv path (`gray codex-sub account …`), where stdin/out
/// are the user's terminal and `codex login` can run interactively.
pub fn command(args: &[String], allow_login: bool) -> String {
    match args.first().map(String::as_str) {
        Some("add") => add(&args[1..], allow_login),
        Some("use") => use_account(&args[1..]),
        Some("remove") | Some("rm") => remove(&args[1..]),
        Some("list") | None => format!("{}\n\n{ACCOUNT_USAGE}", list(&load())),
        Some("help") | Some("--help") | Some("-h") => ACCOUNT_USAGE.to_string(),
        Some(other) => format!("unknown account verb {other:?}\n\n{ACCOUNT_USAGE}"),
    }
}
