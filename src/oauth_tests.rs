use super::*;

fn test_access_token(account_id: &str) -> String {
    use base64::Engine;
    let payload = serde_json::json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
    format!("header.{encoded}.signature")
}

#[test]
fn refreshed_material_keeps_its_expiry() {
    let token: TokenResponse = serde_json::from_value(serde_json::json!({
        "access_token": test_access_token("acct-1"),
        "expires_in": 3600,
    }))
    .unwrap();
    let material = material_from_token(token).unwrap();
    let expiry = material.expires_at.expect("token expiry");
    assert!(expiry > now_secs());
    // Regression: the expiry check must not strip `expires_at` — the host
    // reads a missing expiry as a non-expiring credential and would send
    // the dead access token forever instead of refreshing.
    require_unexpired(&material).unwrap();
    assert_eq!(material.expires_at, Some(expiry));
}

#[test]
fn already_expired_material_is_rejected() {
    let material = CredentialMaterial {
        secrets: SecretMap::default(),
        metadata: Default::default(),
        expires_at: Some(1),
    };
    match require_unexpired(&material) {
        Err(ProviderRpcError::Rpc(failure)) => assert_eq!(failure.code, "invalid_grant"),
        other => panic!("expected invalid_grant, got {other:?}"),
    }
}

#[test]
fn pkce_s256_matches_rfc_7636_vector() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        pkce_challenge(verifier).unwrap(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn callback_path_and_exact_state_are_required() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = bind_listener([0, 0]).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let uri = redirect_uri_for(port, CALLBACK_PATH).unwrap();
        let target = format!("{CALLBACK_PATH}?state=wrong&code=code-test");
        let failed = callback_once(listener, &target, "state-test").await;
        assert_eq!(
            failed.map_err(|error| error.code),
            Err("invalid_request".to_string())
        );

        let listener = bind_listener([0, 0]).await.unwrap();
        let _port = listener.local_addr().unwrap().port();
        let target = format!("{CALLBACK_PATH}?state=state-test&code=code-test");
        let code = callback_once(listener, &target, "state-test")
            .await
            .expect("exact state callback should succeed");
        assert_eq!(code, "code-test");
        assert!(!uri.as_str().contains("state"));
    });
}

async fn callback_once(
    listener: TcpListener,
    target: &str,
    state: &str,
) -> std::result::Result<String, CallbackError> {
    let port = listener.local_addr().unwrap().port();
    let state = state.to_string();
    let handle = tokio::spawn(async move {
        run_callback_once(
            listener,
            CALLBACK_PATH,
            state.as_str(),
            CancellationToken::new(),
        )
        .await
    });
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("callback task finished")
        .unwrap()
}
