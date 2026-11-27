//! The exact protocol-1.2 Codex provider declaration.

use gray_plugin::{
    AuthMethodDecl, PROVIDER_CREDENTIALS, ProviderDecl, ProviderHeaderDecl,
    ProviderHeaderSourceDecl, ProviderRequestPolicyDecl, ProviderTransportDecl,
};

pub const PLUGIN_NAME: &str = "codex-sub";
/// Derived from Cargo.toml — a pinned literal here drifted behind
/// release bumps once already (v0.1.3 shipped a "0.1.2" manifest).
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROVIDER_ID: &str = "codex";
pub const AUTH_METHOD_ID: &str = "chatgpt-subscription";
/// The operator command: `/codex tools …` owns the tool allowlist.
pub const CODEX_COMMAND: &str = "/codex";
pub const CHATGPT_COMMAND: &str = "/chatgpt";

const OPERATOR_COMPLETIONS: &[(&str, &str)] = &[
    (
        "status",
        "show Codex/ChatGPT login, plan, and quota windows",
    ),
    ("tools", "show or set upstream harness tool allowlist"),
    ("tools all", "allow every harness tool the host sends"),
    ("tools bash", "allow bash only"),
    ("account", "manage Codex/ChatGPT logins"),
    ("account list", "list Codex/ChatGPT accounts"),
    ("account usage", "show Codex/ChatGPT account quota usage"),
    ("account use", "pin a Codex/ChatGPT account"),
    ("account remove", "remove a Codex/ChatGPT account"),
    ("account add", "register an already-logged-in account"),
];

pub fn sync_completion_file() {
    let Some(home) = gray_core::paths::gray_home() else {
        return;
    };
    let entries: Vec<serde_json::Value> = OPERATOR_COMPLETIONS
        .iter()
        .map(|(arg, desc)| serde_json::json!({"arg": arg, "description": desc}))
        .collect();
    let Ok(bytes) = serde_json::to_vec(&serde_json::json!({"completion": entries})) else {
        return;
    };
    let plugins_dir = home.join("plugins");
    let _ = std::fs::create_dir_all(&plugins_dir);
    let path = plugins_dir.join(format!("{PLUGIN_NAME}-completion.json"));
    let tmp = plugins_dir.join(format!("{PLUGIN_NAME}-completion.json.tmp"));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn manifest() -> gray_plugin::Manifest {
    sync_completion_file();
    gray_plugin::Manifest {
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        tools: Vec::new(),
        commands: vec![CODEX_COMMAND.to_string(), CHATGPT_COMMAND.to_string()],
        hooks: Vec::new(),
        protocol: Some("1.2".to_string()),
        subcommands: Vec::new(),
        capabilities: vec![PROVIDER_CREDENTIALS.to_string()],
        providers: vec![provider(), plan_provider()],
        provider_errors: Vec::new(),
    }
}

pub fn run_command(name: &str, argv: &[String]) -> Option<serde_json::Value> {
    if name == CODEX_COMMAND || name == CHATGPT_COMMAND {
        if argv.is_empty() {
            return Some(serde_json::json!({}));
        }
        return Some(serde_json::json!({
            "text": crate::settings::command(argv),
        }));
    }
    None
}

/// Codex provider. Chat requests go to the loopback relay the sidecar
/// opens per turn (see `provider/chat`); the relay drives a pooled
/// `codex app-server` child that holds the real ChatGPT session. The
/// bearer is a per-turn relay token minted by the sidecar, never the
/// user's credential — that stays inside the Codex CLI's own home. The
/// base URL is a placeholder — the real per-turn URL comes back in
/// `provider/chat`'s `relay_url`.
pub fn provider() -> ProviderDecl {
    ProviderDecl {
        id: PROVIDER_ID.to_string(),
        name: "Codex (ChatGPT sign-in via Codex CLI)".to_string(),
        transport: ProviderTransportDecl {
            kind: "openai-responses".to_string(),
            base_url: "https://127.0.0.1:1/"
                .parse()
                .expect("loopback placeholder"),
            authorization: gray_plugin::ProviderAuthorizationDecl {
                kind: "bearer".to_string(),
                secret_name: "relay_token".to_string(),
            },
            request: ProviderRequestPolicyDecl {
                // The prompt cache lives inside each pooled app-server
                // child; a host-side cache key would name nothing here.
                prompt_cache_key: false,
                // `warm_replay` must stay off: the transport is a per-turn
                // loopback relay that admits exactly one request — a host
                // replay would hit a dead port or get ADMISSION_CONSUMED.
                warm_replay: false,
                store: false,
                include_reasoning_encrypted: true,
                previous_response_id: false,
                tool_choice: Some("auto".to_string()),
                parallel_tool_calls: Some(true),
                text_verbosity: Some("low".to_string()),
                // Codex's thread cache outlives a turn by ~minutes; the
                // pool refreshes upstream contact at 10min staleness
                // (session.rs KEEPALIVE_AFTER), so ~10min is the honest
                // declared lifetime.
                cache_ttl_secs: Some(600),
            },
            headers: vec![ProviderHeaderDecl {
                name: "session-id".to_string(),
                value: None,
                source: Some(ProviderHeaderSourceDecl::SessionId),
                required: true,
            }],
        },
        auth_methods: vec![AuthMethodDecl {
            id: AUTH_METHOD_ID.to_string(),
            name: "ChatGPT sign-in via `codex login`".to_string(),
            kind: "oauth".to_string(),
            operations: vec![
                "login".to_string(),
                "refresh".to_string(),
                "revoke".to_string(),
                "models".to_string(),
                "chat".to_string(),
            ],
        }],
    }
}

pub fn auth_method() -> AuthMethodDecl {
    provider()
        .auth_methods
        .into_iter()
        .find(|method| method.id == AUTH_METHOD_ID)
        .expect("declared auth method")
}

#[path = "manifest_tests.rs"]
#[cfg(test)]
mod tests;
pub fn plan_provider() -> ProviderDecl {
    ProviderDecl {
        id: "chatgpt".to_string(),
        name: "ChatGPT plan".to_string(),
        transport: ProviderTransportDecl {
            kind: "openai-responses".to_string(),
            base_url: "https://api.openai.com/v1".parse().expect("resource URL"),
            authorization: gray_plugin::ProviderAuthorizationDecl {
                kind: "bearer".to_string(),
                secret_name: "access_token".to_string(),
            },
            request: ProviderRequestPolicyDecl {
                prompt_cache_key: true,
                warm_replay: false,
                store: false,
                include_reasoning_encrypted: true,
                previous_response_id: false,
                tool_choice: None,
                parallel_tool_calls: None,
                text_verbosity: None,
                cache_ttl_secs: None,
            },
            headers: Vec::new(),
        },
        auth_methods: vec![AuthMethodDecl {
            id: "chatgpt-plan".to_string(),
            name: "Sign in with ChatGPT".to_string(),
            kind: "oauth".to_string(),
            operations: vec![
                "login".to_string(),
                "refresh".to_string(),
                "revoke".to_string(),
                "models".to_string(),
            ],
        }],
    }
}
