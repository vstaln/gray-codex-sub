//! The exact protocol-1.2 Codex provider declaration.

use gray_plugin::{
    AuthMethodDecl, PROVIDER_CREDENTIALS, ProviderDecl, ProviderHeaderDecl,
    ProviderHeaderSourceDecl, ProviderRequestPolicyDecl, ProviderTransportDecl,
};

pub const PLUGIN_NAME: &str = "codex-sub";
pub const PLUGIN_VERSION: &str = "0.1.0";
pub const PROVIDER_ID: &str = "codex";
pub const AUTH_METHOD_ID: &str = "chatgpt-subscription";

/// A protocol-1.2 manifest value. Gray validates every provider and
/// malformed peers independently; this plugin ships exactly one valid
/// provider with the reference request policy.
pub fn manifest() -> gray_plugin::Manifest {
    gray_plugin::Manifest {
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        tools: Vec::new(),
        commands: Vec::new(),
        hooks: Vec::new(),
        protocol: Some("1.2".to_string()),
        subcommands: Vec::new(),
        capabilities: vec![PROVIDER_CREDENTIALS.to_string()],
        providers: vec![provider()],
        provider_errors: Vec::new(),
    }
}

/// Codex provider. Requests go to the pinned ChatGPT backend; the host
/// adds bearer, policy, and every declared header from this declaration.
pub fn provider() -> ProviderDecl {
    ProviderDecl {
        id: PROVIDER_ID.to_string(),
        name: "Codex backend (unofficial)".to_string(),
        transport: ProviderTransportDecl {
            kind: "openai-responses".to_string(),
            base_url: "https://chatgpt.com/backend-api/codex"
                .parse()
                .expect("pinned Codex base URL"),
            authorization: gray_plugin::ProviderAuthorizationDecl {
                kind: "bearer".to_string(),
                secret_name: "access_token".to_string(),
            },
            request: ProviderRequestPolicyDecl {
                // The official Codex CLI sends prompt_cache_key =
                // conversation_id on every request; the host fills it
                // with its session id when the policy is on.
                prompt_cache_key: true,
                // `warm_replay` opts the provider into host-side verbatim
                // cache-warm replay: this transport is a real HTTPS endpoint
                // the host calls itself (no relay child), and prompt_cache_key
                // pins the session's cache shard.
                warm_replay: true,
                store: false,
                include_reasoning_encrypted: true,
                previous_response_id: false,
                tool_choice: Some("auto".to_string()),
                parallel_tool_calls: Some(true),
                text_verbosity: Some("low".to_string()),
            },
            headers: vec![
                ProviderHeaderDecl {
                    name: "chatgpt-account-id".to_string(),
                    value: None,
                    source: Some(ProviderHeaderSourceDecl::Metadata {
                        name: "account_id".to_string(),
                    }),
                    required: true,
                },
                ProviderHeaderDecl {
                    name: "originator".to_string(),
                    value: Some("gray".to_string()),
                    source: None,
                    required: false,
                },
                ProviderHeaderDecl {
                    name: "OpenAI-Beta".to_string(),
                    value: Some("responses=experimental".to_string()),
                    source: None,
                    required: false,
                },
                ProviderHeaderDecl {
                    name: "session-id".to_string(),
                    value: None,
                    source: Some(ProviderHeaderSourceDecl::SessionId),
                    required: true,
                },
                ProviderHeaderDecl {
                    name: "x-client-request-id".to_string(),
                    value: None,
                    source: Some(ProviderHeaderSourceDecl::SessionId),
                    required: true,
                },
            ],
        },
        auth_methods: vec![AuthMethodDecl {
            id: AUTH_METHOD_ID.to_string(),
            name: "ChatGPT login via Codex CLI client (unofficial)".to_string(),
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
