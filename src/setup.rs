//! Process setup for `codex app-server` children: binary resolution,
//! child environment, the sidecar's CODEX_HOME, and the credential cache
//! that lets `provider/chat` run without ever receiving the OAuth
//! material on the wire.
//!
//! Why a cache at all: the host sends the credential only on
//! `provider/auth/*` and `provider/models` calls — `provider/chat`
//! carries none. Each sighting is mirrored to `$CODEX_HOME/
//! gray-auth.json` (0600) so a later `provider/chat` (or a restarted
//! sidecar) can still authenticate the app-server child. The child itself
//! never reads this file: it is logged in through `account/login/start`
//! `chatgptAuthTokens`, which makes auth external — the app-server then
//! asks *us* (`account/chatgptAuthTokens/refresh` server request) when a
//! token dies mid-turn, and we answer from this cache, refreshing through
//! the OAuth token endpoint first when the access token is stale.
//!
//! The file deliberately is NOT codex's own `auth.json`: Codex rewrites
//! `auth.json` on its internal refreshes, and mixing writers would lose
//! rotations. External auth keeps ownership unambiguous.

use std::path::PathBuf;
use std::sync::Mutex;

use gray_core::credential::CredentialMaterial;

/// `codex` is missing (or not on PATH): install hint, never a spawn panic.
pub const INSTALL_HINT: &str = "`codex` not found on PATH. Install the Codex CLI (>= 0.154), then \
    retry. Override the binary with GRAY_CODEX_SUB_COMMAND=/path/to/codex.";
/// No credential has been seen yet: the chat path fails closed.
pub const LOGIN_HINT: &str = "No Codex/ChatGPT credential is cached yet. Complete `/connect codex` (or wait for a \
    provider/models refresh) so the sidecar can authenticate the app-server child.";

fn env_override() -> Option<String> {
    std::env::var("GRAY_CODEX_SUB_COMMAND")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Resolve the `codex` binary: explicit override, then PATH.
pub fn resolve_command() -> Option<String> {
    if let Some(v) = env_override() {
        return Some(v);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["codex", "codex.exe", "codex.cmd"] {
            let p: PathBuf = dir.join(name);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// The sidecar's CODEX_HOME: honors an inherited `CODEX_HOME` (the host
/// may pin it), else `~/.gray/codex-home` — separate from the user's own
/// `~/.codex` so our app-server children never disturb a real CLI login.
pub fn codex_home() -> PathBuf {
    if let Some(v) = std::env::var_os("CODEX_HOME")
        && !v.is_empty()
    {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".gray").join("codex-home")
}

/// Env keys that would override auth or session behavior: strip them so
/// the child's only credential is the external-auth login we hand it.
const CONFLICTING_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_HOME",
];

/// Child env for every spawn: never inherit a conflicting value; the
/// resolved CODEX_HOME is always written explicitly.
pub fn child_env(codex_home: &std::path::Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !CONFLICTING_KEYS.contains(&k.as_str()))
        .collect();
    env.push((
        "CODEX_HOME".to_string(),
        codex_home.to_string_lossy().into_owned(),
    ));
    env
}

/// What the cache persists: exactly the fields the external-auth login +
/// refresh path needs. Tokens never reach a log.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedAuth {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub plan_type: Option<String>,
}

impl CachedAuth {
    /// Best-effort copy of a host credential sighting.
    pub fn from_material(m: &CredentialMaterial) -> Option<Self> {
        let access = m.secrets.get("access_token")?.to_string();
        let refresh = m.secrets.get("refresh_token").map(|s| s.to_string());
        let account = m.metadata.get("account_id").cloned();
        Some(Self {
            access_token: access,
            refresh_token: refresh,
            account_id: account,
            expires_at: m.expires_at,
            plan_type: crate::oauth::plan_type_from_access_token(m.secrets.get("access_token")?),
        })
    }

    /// The access token still has runway.
    pub fn access_fresh(&self, now: u64, skew: u64) -> bool {
        self.expires_at.is_some_and(|e| e > now + skew)
    }
}

fn cache_path() -> PathBuf {
    codex_home().join("gray-auth.json")
}

/// Serialize writers: auth sightings can land from the main loop
/// (auth/refresh, models) while a session thread answers an
/// `account/chatgptAuthTokens/refresh`.
static CACHE_LOCK: Mutex<()> = Mutex::new(());

/// Read the mirrored credential, if any. A corrupt file is treated as
/// absent rather than fatal (a later sighting rewrites it).
pub fn read_cached() -> Option<CachedAuth> {
    let _g = CACHE_LOCK.lock().ok()?;
    let bytes = std::fs::read(cache_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_auth(mut auth: CachedAuth) {
    let _g = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    // Merge with what's on disk so a material lacking a refresh token or
    // account id never erases one we already saw.
    if let Ok(prev) = std::fs::read(cache_path())
        && let Ok(old) = serde_json::from_slice::<CachedAuth>(&prev)
    {
        if auth.refresh_token.is_none() {
            auth.refresh_token = old.refresh_token;
        }
        if auth.account_id.is_none() {
            auth.account_id = old.account_id;
        }
        if auth.expires_at.is_none() {
            auth.expires_at = old.expires_at;
        }
        if auth.plan_type.is_none() {
            auth.plan_type = old.plan_type;
        }
    }
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::to_vec(&auth).unwrap_or_default();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        use std::io::Write;
        let _ = f.write_all(&body);
    }
}

/// Mirror a host credential sighting to disk (0600).
pub fn observe(m: &CredentialMaterial) {
    if let Some(auth) = CachedAuth::from_material(m) {
        write_auth(auth);
    }
}

/// Write back a credential produced by a sidecar-run refresh grant (the
/// `account/chatgptAuthTokens/refresh` path).
pub fn store(auth: &CachedAuth) {
    write_auth(auth.clone());
}

/// The user's credential is gone: drop the mirror so a revoked login
/// doesn't keep authenticating children.
pub fn clear() {
    let _ = std::fs::remove_file(cache_path());
}
