//! Process setup for `codex app-server` children: binary resolution,
//! child environment, and the sidecar's CODEX_HOME.
//!
//! The sidecar holds no credentials. Sign-in is the Codex CLI's own job
//! (`codex login`); its tokens live in `$CODEX_HOME/auth.json`, which the
//! CLI reads and rotates itself. This plugin never opens that file — the
//! app-server children are authenticated entirely by the CLI's stored
//! login.
use std::path::PathBuf;
use std::process::{Command, Stdio};
/// `codex` is missing (or not on PATH): install hint, never a spawn panic.
pub const INSTALL_HINT: &str = "`codex` not found on PATH. Install the Codex CLI (>= 0.154), then \
    retry. Override the binary with GRAY_CODEX_SUB_COMMAND=/path/to/codex.";
/// No usable sign-in: the CLI owns the login, the sidecar only reports it.
pub const LOGIN_HINT: &str = "Codex CLI is not signed in for this Codex home. Run \
    `CODEX_HOME=$HOME/.gray/codex-home codex login` for the plugin's managed home, or point \
    CODEX_HOME at your existing Codex login. The plugin never sees your ChatGPT/OpenAI tokens.";
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
        // No HOME at all: a cwd-relative `.gray/` would scribble into
        // whatever directory the host ran from (possibly read-only or
        // the user's project). The temp dir is the safer last resort.
        .unwrap_or_else(std::env::temp_dir);
    home.join(".gray").join("codex-home")
}
/// Env keys that would override the CLI's own auth resolution: strip
/// them so the child authenticates exactly like a user-run `codex`.
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
/// `codex --version`, first non-empty line. `None` when unavailable.
pub fn cli_version() -> Option<String> {
    let bin = resolve_command()?;
    let out = Command::new(&bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}
/// The CLI's own login file under the managed (or pinned) Codex home —
/// an existence check only, used for a fast pre-chat error. Its bytes
/// are never read by this plugin; `account/read` is the authority.
pub fn cli_login_present() -> bool {
    login_present(&codex_home())
}
/// Same check for one credential home — the multi-account pool asks per
/// account, the zero-config path asks for the resolved home.
pub fn login_present(home: &std::path::Path) -> bool {
    home.join("auth.json").is_file() || home.join("config.toml").is_file()
}
/// Legacy mirror of borrowed ChatGPT tokens written by versions <= 0.2.0.
/// Reported, never read; removed only on an explicit user revoke.
pub fn legacy_auth_file() -> PathBuf {
    codex_home().join("gray-auth.json")
}
