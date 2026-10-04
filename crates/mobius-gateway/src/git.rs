//! Noninteractive Git environment shared by workspace and host operations.

pub(crate) const GIT_ENVIRONMENT: [(&str, &str); 4] = [
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("LC_ALL", "C"),
];
// These variables can redirect Git to an untrusted repository or inject command-scoped settings.
pub(crate) const REPOSITORY_LOCAL_GIT_ENVIRONMENT: [&str; 17] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

#[derive(Clone, Copy)]
pub(crate) enum Environment {
    Inherited,
    Isolated,
}

pub(crate) fn command(environment: Environment) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    if matches!(environment, Environment::Isolated) {
        command.env_clear();
    }
    command.envs(GIT_ENVIRONMENT);
    for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_environments_disable_interaction_and_remove_repository_redirection() {
        for environment in [Environment::Inherited, Environment::Isolated] {
            let command = command(environment);
            assert_eq!(
                command
                    .get_envs()
                    .find(|(name, _)| *name == "GIT_TERMINAL_PROMPT")
                    .and_then(|(_, value)| value),
                Some(std::ffi::OsStr::new("0")),
            );
            for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT {
                assert!(
                    command
                        .get_envs()
                        .find(|(key, _)| *key == name)
                        .and_then(|(_, value)| value)
                        .is_none()
                );
            }
        }
    }
}
