//! Shared terminal frontends.

pub mod command;
pub mod frontend;

/// Adds optional WebSocket admission only to its explicitly configured endpoint.
/// # Errors
///
/// Returns an error when the bearer endpoint/token configuration is incomplete or invalid.
pub fn authenticated_endpoint(
    endpoint: mobius_gateway::client::Endpoint,
) -> mobius_gateway::Result<mobius_gateway::client::Endpoint> {
    use std::env::{VarError, var};

    match (var("MOBIUS_GATEWAY_BEARER_ENDPOINT"), var("MOBIUS_GATEWAY_BEARER_TOKEN")) {
        (Err(VarError::NotPresent), Err(VarError::NotPresent)) => Ok(endpoint),
        (Ok(scope), Ok(token)) => scoped_bearer(endpoint, &scope, &token),
        _ => Err(mobius_gateway::Error::Config(
            "set both MOBIUS_GATEWAY_BEARER_ENDPOINT and MOBIUS_GATEWAY_BEARER_TOKEN for WebSocket admission".into(),
        )),
    }
}

fn scoped_bearer(
    endpoint: mobius_gateway::client::Endpoint,
    scope: &str,
    token: &str,
) -> mobius_gateway::Result<mobius_gateway::client::Endpoint> {
    let scope: mobius_gateway::client::Endpoint = scope.parse()?;
    if !scope.is_websocket() {
        return Err(mobius_gateway::Error::Config(
            "WebSocket admission requires a wss:// endpoint".into(),
        ));
    }
    if endpoint == scope {
        endpoint.with_websocket_bearer(token)
    } else {
        Ok(endpoint)
    }
}

/// Converts a gateway transport/protocol failure without losing its diagnostic text.
pub fn gateway_error(error: mobius_gateway::Error) -> mobius::Error {
    mobius::Error::Stopped(error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn websocket_admission_is_scoped_to_the_explicit_secure_endpoint() {
        let scope = "wss://gateway.example";
        let endpoint: mobius_gateway::client::Endpoint = scope.parse().expect("endpoint");
        let authenticated =
            super::scoped_bearer(endpoint.clone(), "wss://gateway.example:443", "secret")
                .expect("admission");
        assert_ne!(authenticated, endpoint);
        assert_eq!(authenticated.to_string(), scope);
        assert!(!format!("{authenticated:?}").contains("secret"));
        for endpoint in [
            "wss://another.example",
            "wss://gateway.example.evil.example",
            "wss://gateway.example:8443",
            "tls://gateway.example:443",
            "tcp://127.0.0.1:8741",
        ] {
            let endpoint: mobius_gateway::client::Endpoint = endpoint.parse().expect("endpoint");
            let authenticated = super::scoped_bearer(endpoint.clone(), scope, "secret")
                .expect("other endpoints receive no admission credential");
            assert_eq!(authenticated, endpoint);
        }
        assert!(super::scoped_bearer(endpoint, "tcp://127.0.0.1:8741", "secret").is_err());
    }
}
