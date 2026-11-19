use gray_plugin::ProviderRpcFailure;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use super::oauth::{AUTH_BODY_LIMIT, Endpoints, failure, read_bounded};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

pub async fn verify(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    id_token: &str,
    client_id: &str,
    nonce: &str,
) -> Result<Identity, ProviderRpcFailure> {
    let response = http
        .get(endpoints.jwks.clone())
        .send()
        .await
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let body = read_bounded(response, AUTH_BODY_LIMIT)
        .await
        .ok_or_else(unavailable)?;
    let jwks: JwkSet = serde_json::from_slice(&body).map_err(|_| unavailable())?;
    verify_with(&jwks, &endpoints.issuer, id_token, client_id, nonce)
}

pub fn verify_with(
    jwks: &JwkSet,
    issuer: &str,
    id_token: &str,
    client_id: &str,
    nonce: &str,
) -> Result<Identity, ProviderRpcFailure> {
    let header = decode_header(id_token).map_err(|_| invalid())?;
    if header.alg != Algorithm::RS256 {
        return Err(invalid());
    }
    let jwk = header
        .kid
        .as_deref()
        .and_then(|kid| jwks.find(kid))
        .ok_or_else(invalid)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| invalid())?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let claims = decode::<Claims>(id_token, &key, &validation)
        .map_err(|_| invalid())?
        .claims;
    if claims.nonce.as_deref() != Some(nonce) {
        return Err(invalid());
    }
    if claims.sub.is_empty() {
        return Err(invalid());
    }
    Ok(Identity {
        subject: claims.sub,
        email: claims.email.filter(|e| !e.is_empty()),
    })
}

fn invalid() -> ProviderRpcFailure {
    failure(
        "invalid_id_token",
        "ChatGPT sign-in returned an identity Gray could not verify.",
    )
}

fn unavailable() -> ProviderRpcFailure {
    ProviderRpcFailure {
        code: "unavailable".to_string(),
        message: "Could not fetch OpenAI's signing keys; try /connect again.".to_string(),
        retryable: true,
        terminal: false,
    }
}
