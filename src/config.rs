use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::paths::{Paths, secure_created_file};

const DEFAULT_CONFIG: &str = r#"# WIPS keeps agent processes in a private tmux server and records their sessions.
# `default_agent` names one of the presets in `[agents]` below.
default_agent = "codex"

# These names identify WIPS's tmux socket and long-lived tmux session.
[tmux]
socket = "wips"
session = "wips"

# A preset is an executable plus literal arguments. WIPS invokes it directly; no shell
# parses this array. Session identity and resume flags are managed by WIPS and must not
# be included here.
[agents.codex]
kind = "codex"
program = "codex"
args = []
"#;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentKind {
    Codex,
    Claude,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentConfig {
    pub(crate) kind: AgentKind,
    pub(crate) program: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TmuxConfig {
    pub(crate) socket: String,
    pub(crate) session: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) default_agent: String,
    pub(crate) tmux: TmuxConfig,
    pub(crate) agents: BTreeMap<String, AgentConfig>,
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error(
        "{field} `{value}` is invalid; names must start with an ASCII letter or digit and contain only ASCII letters, digits, `-`, or `_`"
    )]
    InvalidName { field: &'static str, value: String },

    #[error("default agent `{0}` does not name a configured agent preset")]
    UnknownDefaultAgent(String),

    #[error("agent preset `{0}` is not configured")]
    UnknownAgent(String),

    #[error("agent preset `{agent}` has an empty program")]
    EmptyProgram { agent: String },

    #[error("agent preset `{agent}` has a program containing a NUL byte")]
    ProgramContainsNul { agent: String },

    #[error("agent preset `{agent}` has an argument containing a NUL byte")]
    ArgumentContainsNul { agent: String },

    #[error(
        "agent preset `{agent}` includes `{argument}`, which conflicts with WIPS-managed session identity"
    )]
    IdentityArgument { agent: String, argument: String },

    #[error(
        "agent preset `{agent}` overrides WIPS-managed hooks with `{argument}`; put custom hooks in a Codex configuration file so hook sources can merge"
    )]
    ManagedHookArgument { agent: String, argument: String },
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_agent: String::from("codex"),
            tmux: TmuxConfig {
                socket: String::from("wips"),
                session: String::from("wips"),
            },
            agents: BTreeMap::from([(
                String::from("codex"),
                AgentConfig {
                    kind: AgentKind::Codex,
                    program: String::from("codex"),
                    args: Vec::new(),
                },
            )]),
        }
    }
}

impl Config {
    pub(crate) fn load_or_create(paths: &Paths) -> Result<Self> {
        paths.ensure_dirs()?;
        let source = read_or_create(&paths.config)
            .with_context(|| format!("could not read configuration {}", paths.config.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("could not parse configuration {}", paths.config.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid configuration {}", paths.config.display()))?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> std::result::Result<(), ConfigError> {
        validate_name("default_agent", &self.default_agent)?;
        validate_name("tmux.socket", &self.tmux.socket)?;
        validate_name("tmux.session", &self.tmux.session)?;

        if !self.agents.contains_key(&self.default_agent) {
            return Err(ConfigError::UnknownDefaultAgent(self.default_agent.clone()));
        }

        for (name, agent) in &self.agents {
            validate_name("agent preset", name)?;
            validate_agent(name, agent)?;
        }

        Ok(())
    }

    pub(crate) fn agent(&self, name: &str) -> std::result::Result<&AgentConfig, ConfigError> {
        self.agents
            .get(name)
            .ok_or_else(|| ConfigError::UnknownAgent(name.to_owned()))
    }
}

fn read_or_create(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_default(path),
        Err(error) => Err(error),
    }
}

fn create_default(path: &Path) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            secure_created_file(path)?;
            file.write_all(DEFAULT_CONFIG.as_bytes())?;
            file.flush()?;
            Ok(String::from(DEFAULT_CONFIG))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => fs::read_to_string(path),
        Err(error) => Err(error),
    }
}

fn validate_name(field: &'static str, value: &str) -> std::result::Result<(), ConfigError> {
    let mut characters = value.chars();
    let starts_validly = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    if !starts_validly
        || !characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ConfigError::InvalidName {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_agent(name: &str, agent: &AgentConfig) -> std::result::Result<(), ConfigError> {
    if agent.program.trim().is_empty() {
        return Err(ConfigError::EmptyProgram {
            agent: name.to_owned(),
        });
    }
    if agent.program.contains('\0') {
        return Err(ConfigError::ProgramContainsNul {
            agent: name.to_owned(),
        });
    }

    for argument in &agent.args {
        if argument.contains('\0') {
            return Err(ConfigError::ArgumentContainsNul {
                agent: name.to_owned(),
            });
        }
        if conflicts_with_identity(agent.kind, argument) {
            return Err(ConfigError::IdentityArgument {
                agent: name.to_owned(),
                argument: argument.clone(),
            });
        }
    }
    if agent.kind == AgentKind::Codex {
        if let Some(argument) = managed_hook_override(&agent.args) {
            return Err(ConfigError::ManagedHookArgument {
                agent: name.to_owned(),
                argument: argument.to_owned(),
            });
        }
    }
    Ok(())
}

fn managed_hook_override(arguments: &[String]) -> Option<&str> {
    for (index, argument) in arguments.iter().enumerate() {
        let value = match argument.as_str() {
            "-c" | "--config" => arguments.get(index + 1).map(String::as_str),
            value => value
                .strip_prefix("--config=")
                .or_else(|| value.strip_prefix("-c="))
                .or_else(|| value.strip_prefix("-c").filter(|value| !value.is_empty())),
        };
        let Some(value) = value else {
            continue;
        };
        let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
        if key == "hooks"
            || ["SessionStart", "UserPromptSubmit", "Stop"]
                .iter()
                .any(|event| {
                    key == format!("hooks.{event}") || key.starts_with(&format!("hooks.{event}."))
                })
        {
            return Some(value);
        }
    }
    None
}

fn conflicts_with_identity(kind: AgentKind, argument: &str) -> bool {
    match kind {
        AgentKind::Claude => {
            matches!(
                argument,
                "--session-id" | "--resume" | "-r" | "--continue" | "-c" | "--fork-session"
            ) || [
                "--session-id=",
                "--resume=",
                "--continue=",
                "--fork-session=",
                "-r=",
            ]
            .iter()
            .any(|prefix| argument.starts_with(prefix))
        }
        AgentKind::Codex => {
            argument == "resume" || argument == "--last" || argument.starts_with("--last=")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn config_with(kind: AgentKind, args: &[&str]) -> Config {
        Config {
            default_agent: String::from("test-agent"),
            tmux: TmuxConfig {
                socket: String::from("wips-test"),
                session: String::from("wips-test"),
            },
            agents: BTreeMap::from([(
                String::from("test-agent"),
                AgentConfig {
                    kind,
                    program: String::from("agent"),
                    args: args
                        .iter()
                        .map(|argument| String::from(*argument))
                        .collect(),
                },
            )]),
        }
    }

    #[test]
    fn default_config_is_typed_and_valid() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG).expect("default TOML should parse");

        assert_eq!(parsed, Config::default());
        parsed.validate().expect("default config should validate");
        assert_eq!(
            parsed.agent("codex").expect("default preset").kind,
            AgentKind::Codex
        );
    }

    #[test]
    fn creates_documented_default_only_when_absent() {
        let temp = tempdir().expect("temporary directory should be created");
        let paths = Paths::new(
            temp.path().join("config/config.toml"),
            temp.path().join("state"),
        );

        let config = Config::load_or_create(&paths).expect("default config should load");
        let source = fs::read_to_string(&paths.config).expect("config should be readable");

        assert_eq!(config, Config::default());
        assert_eq!(source, DEFAULT_CONFIG);
        assert!(source.starts_with("# WIPS"));
    }

    #[test]
    fn preserves_existing_config_exactly() {
        let temp = tempdir().expect("temporary directory should be created");
        let paths = Paths::new(
            temp.path().join("config/config.toml"),
            temp.path().join("state"),
        );
        paths.ensure_dirs().expect("directories should be created");
        let source = r#"# keep this comment
default_agent = "claude"

[tmux]
socket = "custom-socket"
session = "custom-session"

[agents.claude]
kind = "claude"
program = "/opt/bin/claude"
args = ["--verbose"]
"#;
        fs::write(&paths.config, source).expect("fixture config should be written");

        let config = Config::load_or_create(&paths).expect("existing config should load");

        assert_eq!(config.default_agent, "claude");
        assert_eq!(
            fs::read_to_string(&paths.config).expect("config should remain readable"),
            source
        );
    }

    #[cfg(unix)]
    #[test]
    fn created_config_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().expect("temporary directory should be created");
        let paths = Paths::new(
            temp.path().join("config/config.toml"),
            temp.path().join("state"),
        );

        Config::load_or_create(&paths).expect("default config should load");

        let mode = fs::metadata(&paths.config)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn validates_all_names() {
        for (field, value) in [
            ("default_agent", ""),
            ("default_agent", "bad name"),
            ("tmux.socket", "socket/name"),
            ("tmux.session", ".hidden"),
            ("agent preset", "agent:name"),
        ] {
            let mut config = Config::default();
            match field {
                "default_agent" => config.default_agent = String::from(value),
                "tmux.socket" => config.tmux.socket = String::from(value),
                "tmux.session" => config.tmux.session = String::from(value),
                "agent preset" => {
                    let agent = config.agents.get("codex").expect("default preset").clone();
                    config.agents.insert(String::from(value), agent);
                }
                _ => unreachable!(),
            }

            assert!(matches!(
                config.validate(),
                Err(ConfigError::InvalidName { field: actual, .. }) if actual == field
            ));
        }
    }

    #[test]
    fn default_agent_must_exist() {
        let config = Config {
            default_agent: String::from("claude"),
            ..Config::default()
        };

        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnknownDefaultAgent(name)) if name == "claude"
        ));
    }

    #[test]
    fn program_must_be_usable() {
        for program in ["", "  ", "agent\0name"] {
            let mut config = Config::default();
            config
                .agents
                .get_mut("codex")
                .expect("default preset")
                .program = String::from(program);

            assert!(matches!(
                config.validate(),
                Err(ConfigError::EmptyProgram { .. } | ConfigError::ProgramContainsNul { .. })
            ));
        }
    }

    #[test]
    fn claude_identity_arguments_are_rejected() {
        for argument in [
            "--session-id",
            "--session-id=00000000-0000-0000-0000-000000000000",
            "--resume",
            "--resume=last",
            "-r",
            "-r=last",
            "--continue",
            "-c",
            "--fork-session",
        ] {
            let config = config_with(AgentKind::Claude, &[argument]);
            assert!(matches!(
                config.validate(),
                Err(ConfigError::IdentityArgument { argument: actual, .. }) if actual == argument
            ));
        }
    }

    #[test]
    fn codex_identity_arguments_are_rejected() {
        for argument in ["resume", "--last", "--last=true"] {
            let config = config_with(AgentKind::Codex, &[argument]);
            assert!(matches!(
                config.validate(),
                Err(ConfigError::IdentityArgument { argument: actual, .. }) if actual == argument
            ));
        }
    }

    #[test]
    fn codex_managed_hook_cli_overrides_are_rejected() {
        for arguments in [
            vec!["-c", "hooks.Stop=[]"],
            vec!["--config", "hooks.SessionStart.0={}"],
            vec!["--config=hooks.UserPromptSubmit=[]"],
            vec!["-chooks.Stop=[]"],
            vec!["-c", "hooks={Stop=[]}"],
        ] {
            let config = config_with(AgentKind::Codex, &arguments);
            assert!(matches!(
                config.validate(),
                Err(ConfigError::ManagedHookArgument { .. })
            ));
        }
    }

    #[test]
    fn unrelated_agent_arguments_are_allowed() {
        config_with(AgentKind::Claude, &["--model", "opus"])
            .validate()
            .expect("Claude model arguments should be allowed");
        config_with(AgentKind::Codex, &["--model", "gpt-5.5"])
            .validate()
            .expect("Codex model arguments should be allowed");
        config_with(AgentKind::Codex, &["-c", "hooks.Notification=[]"])
            .validate()
            .expect("unmanaged Codex hooks should be allowed");
    }

    #[test]
    fn named_agent_lookup_is_typed() {
        let config = Config::default();

        assert_eq!(
            config.agent("codex").expect("default preset").kind,
            AgentKind::Codex
        );
        assert!(matches!(
            config.agent("missing"),
            Err(ConfigError::UnknownAgent(name)) if name == "missing"
        ));
    }
}
