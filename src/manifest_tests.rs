use super::*;

#[test]
fn manifest_matches_protocol_12_contract() {
    let manifest = manifest();
    assert_eq!(manifest.name, PLUGIN_NAME);
    assert_eq!(manifest.version, PLUGIN_VERSION);
    assert_eq!(manifest.protocol.as_deref(), Some("1.2"));
    assert_eq!(
        manifest.capabilities,
        vec![PROVIDER_CREDENTIALS.to_string()]
    );
    assert_eq!(manifest.providers.len(), 1);
    let provider = &manifest.providers[0];
    assert_eq!(provider.id, PROVIDER_ID);
    assert_eq!(
        provider.transport.base_url.as_str(),
        "https://chatgpt.com/backend-api/codex"
    );
    assert_eq!(provider.transport.authorization.secret_name, "access_token");
    let binding = provider.profile_binding(AUTH_METHOD_ID).unwrap();
    assert!(binding.starts_with("sha256:"));
    assert_eq!(
        provider.profile_binding(AUTH_METHOD_ID).unwrap(),
        binding,
        "profile binding must be stable for the same declared transport"
    );
    let method = &provider.auth_methods[0];
    assert_eq!(method.id, AUTH_METHOD_ID);
    assert_eq!(method.kind, "oauth");
    assert_eq!(
        method.operations,
        vec!["login", "refresh", "revoke", "models"]
    );
}

// Moved from gray core's `codex_plugin_manifest_passes_host_protocol_validation`
// (`crates/gray/src/providers/tests.rs`): the same host-side validation now
// lives next to the declaration it checks.
#[test]
fn manifest_passes_host_protocol_validation() {
    let manifest = manifest();
    assert_eq!(manifest.name, "codex-sub");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.protocol.as_deref(), Some("1.2"));
    assert_eq!(
        manifest.capabilities,
        vec![PROVIDER_CREDENTIALS.to_string()]
    );
    assert_eq!(manifest.providers.len(), 1);
    let provider = &manifest.providers[0];
    assert_eq!(provider.id, "codex");
    assert_eq!(
        provider.transport.base_url.as_str(),
        "https://chatgpt.com/backend-api/codex"
    );
    provider
        .validate()
        .expect("codex provider declaration must pass host validation");
    let method = &provider.auth_methods[0];
    assert_eq!(method.id, "chatgpt-subscription");
    assert_eq!(method.kind, "oauth");
    assert_eq!(
        provider.profile_binding(&method.id).unwrap(),
        provider.profile_binding(&method.id).unwrap()
    );
    assert!(
        provider
            .profile_binding(&method.id)
            .unwrap()
            .starts_with("sha256:")
    );
}
