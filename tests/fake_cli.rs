//! Fake `codex app-server` over the real stdio JSON-RPC wire. Probes are
//! exercised against this stand-in since no live ChatGPT login exists on
//! the test machine. The real server's method coverage is pinned by the
//! shipped v2 schema; its exact auth-error text is unverified.
#![cfg(unix)]
use codex_sub::appserver::{self, Probe};
use codex_sub::usage;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
static ENV_LOCK: Mutex<()> = Mutex::new(());
const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
log = os.environ.get("FAKE_CODEX_LOG")
if log:
    with open(log, "a") as f:
        f.write(json.dumps(sys.argv[1:]) + " CODEX_HOME=" + os.environ.get("CODEX_HOME", "") + "\n")
if "--version" in sys.argv:
    print("codex-cli 0.0.0-fake")
    sys.exit(0)
if "app-server" not in sys.argv:
    sys.stderr.write("fake codex: expected app-server\n")
    sys.exit(2)
mode = os.environ.get("FAKE_CODEX_MODE", "ready")
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if "id" not in req:
        continue
    rid, method = req["id"], req.get("method")
    if method == "initialize":
        out = {"serverInfo": {"name": "fake", "version": "0"}}
    elif method == "account/read":
        if mode == "unauth":
            out = {"account": None, "requiresOpenaiAuth": True}
        else:
            out = {"account": {"type": "chatgpt", "email": "f@example.com",
                               "planType": "pro"}, "requiresOpenaiAuth": True}
    elif method == "account/rateLimits/read":
        if mode == "unauth":
            out = None
            print(json.dumps({"id": rid,
                              "error": {"code": -32000, "message": "not logged in"}}))
            sys.stdout.flush()
            continue
        out = {"rateLimits": {
            "primary": {"usedPercent": 42, "windowDurationMins": 300, "resetsAt": 1899999999},
            "secondary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": 1900000000},
            "planType": "pro", "rateLimitReachedType": None,
            "credits": {"hasCredits": True, "unlimited": False, "balance": "12.34"}}}
    elif method == "model/list":
        page = (req.get("params") or {}).get("cursor")
        if not page:
            out = {"data": [
                {"id": "x1", "model": "fake-a", "displayName": "Fake A", "hidden": False,
                 "isDefault": True, "description": "", "defaultReasoningEffort": "medium",
                 "supportedReasoningEfforts": [{"description": "d", "reasoningEffort": "low"},
                                                {"description": "d", "reasoningEffort": "high"}]},
                {"id": "x2", "model": "fake-hidden", "displayName": "Hidden", "hidden": True,
                 "isDefault": False, "description": "", "defaultReasoningEffort": "low",
                 "supportedReasoningEfforts": []}],
                "nextCursor": "p2"}
        else:
            out = {"data": [
                {"id": "x3", "model": "fake-b", "displayName": "Fake B", "hidden": False,
                 "isDefault": False, "description": "", "defaultReasoningEffort": "low",
                 "supportedReasoningEfforts": []}],
                "nextCursor": None}
    else:
        out = {}
    print(json.dumps({"id": rid, "result": out}))
    sys.stdout.flush()
"#;
struct Fake {
    dir: PathBuf,
}
impl Fake {
    fn new(tag: &str) -> Fake {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("codex-sub-fake-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("codex");
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(FAKE.as_bytes()).unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Fake { dir }
    }
    fn bin(&self) -> String {
        self.dir.join("codex").to_string_lossy().into_owned()
    }
    fn home(&self) -> String {
        self.dir.join("home").to_string_lossy().into_owned()
    }
    fn log(&self) -> PathBuf {
        self.dir.join("argv.log")
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn with_fake<T>(mode: &str, f: impl FnOnce(&Fake) -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake = Fake::new(mode);
    // SAFETY: ENV_LOCK serializes every env mutation in this binary;
    // the child env is read at spawn time only.
    unsafe {
        std::env::set_var("GRAY_CODEX_SUB_COMMAND", fake.bin());
        std::env::set_var("CODEX_HOME", fake.home());
        std::env::set_var("FAKE_CODEX_MODE", mode);
        std::env::set_var("FAKE_CODEX_LOG", fake.log());
    }
    let out = f(&fake);
    unsafe {
        std::env::remove_var("GRAY_CODEX_SUB_COMMAND");
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("FAKE_CODEX_MODE");
        std::env::remove_var("FAKE_CODEX_LOG");
    }
    out
}
#[test]
fn ready_probe_reports_the_cli_account() {
    with_fake("ready", |fake| match appserver::probe() {
        Probe::Ready(v) => {
            assert_eq!(v["account"]["type"], "chatgpt");
            let argv = std::fs::read_to_string(fake.log()).unwrap();
            assert!(argv.contains("app-server"), "argv: {argv}");
            assert!(
                argv.contains(&format!("CODEX_HOME={}", fake.home())),
                "env: {argv}"
            );
        }
        other => panic!("expected Ready, got {other:?}"),
    });
}
#[test]
fn signed_out_probe_reports_not_signed_in() {
    with_fake("unauth", |_| match appserver::probe() {
        Probe::NotSignedIn(_) => {}
        other => panic!("expected NotSignedIn, got {other:?}"),
    });
}
#[test]
fn models_follow_paged_model_list_and_skip_hidden() {
    with_fake("ready", |_| {
        let catalog = codex_sub::models::fetch().expect("models");
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fake-a", "fake-b"]);
        assert_eq!(
            catalog.models[0].reasoning_efforts,
            vec!["low".to_string(), "high".to_string()]
        );
    });
}
#[test]
fn usage_card_maps_rate_limits_and_credits() {
    with_fake("ready", |_| {
        let card = usage::handle().expect("usage");
        assert_eq!(card["available"], true);
        assert_eq!(card["plan"], "Pro");
        let windows = card["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0]["id"], "primary_five_hour");
        assert_eq!(windows[0]["used_percent"], 42.0);
        assert!(card["note"].as_str().unwrap().contains("12.34"));
    });
    with_fake("unauth", |_| {
        assert!(usage::handle().is_err());
    });
}
#[test]
fn missing_binary_is_not_installed() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: same ENV_LOCK serialization as with_fake.
    unsafe {
        std::env::set_var("GRAY_CODEX_SUB_COMMAND", "/nonexistent/codex");
    }
    match appserver::probe() {
        Probe::Failed(e) => assert!(e.contains("spawn"), "{e}"),
        other => panic!("expected spawn failure, got {other:?}"),
    }
    unsafe {
        std::env::remove_var("GRAY_CODEX_SUB_COMMAND");
    }
}
