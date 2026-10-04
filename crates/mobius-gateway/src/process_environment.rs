//! Network environment shared by trusted dependency installers.

use std::ffi::OsString;

use tokio::process::Command;

pub(crate) const NETWORK_ENVIRONMENT: [&str; 10] = [
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
];

pub(crate) fn forward_network_environment(
    command: &mut Command,
    get: impl Fn(&str) -> Option<OsString>,
) {
    for name in NETWORK_ENVIRONMENT {
        if let Some(value) = get(name) {
            command.env(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_forwarding_includes_all_proxy_and_excludes_credentials() {
        let mut command = Command::new("fixture");
        command.env_clear();
        forward_network_environment(&mut command, |name| {
            ["ALL_PROXY", "all_proxy", "SSL_CERT_FILE", "OPENAI_API_KEY"]
                .contains(&name)
                .then(|| OsString::from("fixture"))
        });
        let names = command
            .as_std()
            .get_envs()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert!(names.contains(&std::ffi::OsStr::new("ALL_PROXY")));
        assert!(names.contains(&std::ffi::OsStr::new("all_proxy")));
        assert!(names.contains(&std::ffi::OsStr::new("SSL_CERT_FILE")));
        assert!(!names.contains(&std::ffi::OsStr::new("OPENAI_API_KEY")));
    }
}
