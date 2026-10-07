use super::*;

use std::time::Duration;

fn test_config(ports: [u16; 2]) -> OAuthConfig {
    OAuthConfig {
        client_id: "client-test".to_string(),
        callback_path: oauth::CALLBACK_PATH.to_string(),
        callback_ports: ports,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn poll_unknown_operation_is_lost() {
    runtime().block_on(async {
        let logins = LoginManager::default();
        assert!(matches!(
            logins.poll("missing").await,
            ProviderAuthPoll::OperationLost
        ));
    });
}

#[test]
fn pending_login_polls_until_cancelled() {
    runtime().block_on(async {
        let logins = LoginManager::default();
        let start = logins
            .start(&test_config([0, 0]))
            .await
            .expect("login start");
        assert!(matches!(
            logins.poll(&start.operation_id).await,
            ProviderAuthPoll::Pending { .. }
        ));
        logins.cancel(&start.operation_id).await;
        assert!(matches!(
            logins.poll(&start.operation_id).await,
            ProviderAuthPoll::OperationLost
        ));
    });
}

#[test]
fn expired_poll_reclaims_the_callback_port() {
    runtime().block_on(async {
        let port = free_port();
        let logins = LoginManager::default();
        let start = logins
            .start(&test_config([port, port]))
            .await
            .expect("login start");
        // The pending login's callback listener holds the port.
        assert!(oauth::bind_listener([port, port]).await.is_err());
        logins.force_expired(&start.operation_id).await;
        assert!(matches!(
            logins.poll(&start.operation_id).await,
            ProviderAuthPoll::OperationLost
        ));
        // Poll reclaimed the operation: aborting the callback task drops
        // the listener and the port comes back.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if oauth::bind_listener([port, port]).await.is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expired poll must free the callback port"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
}
