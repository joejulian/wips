use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::config::{AgentConfig, AgentKind};
use crate::model::Session;

const WIPS_HOOK_COMMAND: &str = r#""$WIPS_EXECUTABLE" hook"#;
const CODEX_HOOK_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "Stop"];
const CLAUDE_HOOK_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "Stop"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: BTreeMap<OsString, OsString>,
    pub(crate) cwd: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LaunchContext<'a> {
    pub(crate) logical_session_id: &'a str,
    pub(crate) tmux_socket: &'a str,
    pub(crate) tmux_session: &'a str,
    pub(crate) current_exe: &'a Path,
    pub(crate) state_dir: &'a Path,
    pub(crate) config_path: &'a Path,
    pub(crate) claude_settings_path: &'a Path,
}

#[derive(Debug, Error)]
pub(crate) enum ProviderError {
    #[error("{agent} session is missing its provider session ID")]
    MissingSessionId { agent: &'static str },

    #[error("Claude session ID {id:?} is not a valid UUID")]
    InvalidClaudeSessionId {
        id: String,
        #[source]
        source: uuid::Error,
    },

    #[error("WIPS executable path is not valid UTF-8: {path:?}")]
    NonUtf8Executable { path: PathBuf },

    #[error("could not serialize Claude settings")]
    SerializeClaudeSettings(#[source] serde_json::Error),

    #[error("could not write Claude settings to {path:?}")]
    WriteClaudeSettings {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) fn build_new_command(
    agent: &AgentConfig,
    session: &Session,
    context: &LaunchContext<'_>,
) -> Result<CommandSpec, ProviderError> {
    let mut command = base_command(agent, session, context);

    match &agent.kind {
        AgentKind::Codex => command.args.extend(codex_hook_overrides()),
        AgentKind::Claude => {
            let session_id = claude_session_id(session)?;
            add_claude_settings(&mut command.args, context.claude_settings_path);
            command.args.push(OsString::from("--session-id"));
            command.args.push(OsString::from(session_id));
        }
    }

    Ok(command)
}

pub(crate) fn build_resume_command(
    agent: &AgentConfig,
    session: &Session,
    context: &LaunchContext<'_>,
) -> Result<CommandSpec, ProviderError> {
    let session_id = required_session_id(session, agent.kind)?;
    let mut command = base_command(agent, session, context);

    match &agent.kind {
        AgentKind::Codex => {
            command.args.extend(codex_hook_overrides());
            command.args.push(OsString::from("resume"));
            command.args.push(OsString::from(session_id));
        }
        AgentKind::Claude => {
            validate_claude_session_id(session_id)?;
            add_claude_settings(&mut command.args, context.claude_settings_path);
            command.args.push(OsString::from("--resume"));
            command.args.push(OsString::from(session_id));
        }
    }

    Ok(command)
}

pub(crate) fn codex_hook_overrides() -> Vec<OsString> {
    let mut args = Vec::with_capacity(CODEX_HOOK_EVENTS.len() * 2);

    for event in CODEX_HOOK_EVENTS {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            r#"hooks.{event}=[{{hooks=[{{type="command",command='{WIPS_HOOK_COMMAND}'}}]}}]"#
        )));
    }

    args
}

pub(crate) fn claude_settings_document(current_exe: &Path) -> Result<Value, ProviderError> {
    let command = current_exe
        .to_str()
        .ok_or_else(|| ProviderError::NonUtf8Executable {
            path: current_exe.to_path_buf(),
        })?;

    let hook_group = || {
        json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "args": ["hook"]
            }]
        })
    };
    let hooks = CLAUDE_HOOK_EVENTS
        .into_iter()
        .map(|event| (event.to_owned(), Value::Array(vec![hook_group()])))
        .collect::<serde_json::Map<_, _>>();

    Ok(json!({ "hooks": hooks }))
}

pub(crate) fn write_claude_settings(path: &Path, current_exe: &Path) -> Result<(), ProviderError> {
    let document = claude_settings_document(current_exe)?;
    let mut bytes =
        serde_json::to_vec_pretty(&document).map_err(ProviderError::SerializeClaudeSettings)?;
    bytes.push(b'\n');

    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options
        .open(path)
        .map_err(|source| ProviderError::WriteClaudeSettings {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&bytes)
        .map_err(|source| ProviderError::WriteClaudeSettings {
            path: path.to_path_buf(),
            source,
        })
}

fn base_command(
    agent: &AgentConfig,
    session: &Session,
    context: &LaunchContext<'_>,
) -> CommandSpec {
    let args = agent.args.iter().map(OsString::from).collect();
    let env = [
        (
            OsString::from("WIPS_SESSION_ID"),
            OsString::from(context.logical_session_id),
        ),
        (
            OsString::from("WIPS_EXECUTABLE"),
            context.current_exe.as_os_str().to_owned(),
        ),
        (
            OsString::from("WIPS_STATE_DIR"),
            context.state_dir.as_os_str().to_owned(),
        ),
        (
            OsString::from("WIPS_CONFIG"),
            context.config_path.as_os_str().to_owned(),
        ),
        (
            OsString::from("WIPS_TMUX_SOCKET"),
            OsString::from(context.tmux_socket),
        ),
        (
            OsString::from("WIPS_TMUX_SESSION"),
            OsString::from(context.tmux_session),
        ),
    ]
    .into_iter()
    .collect();

    CommandSpec {
        program: OsString::from(&agent.program),
        args,
        env,
        cwd: session.cwd.clone(),
    }
}

fn add_claude_settings(args: &mut Vec<OsString>, settings_path: &Path) {
    args.push(OsString::from("--settings"));
    args.push(settings_path.as_os_str().to_owned());
}

fn claude_session_id(session: &Session) -> Result<&str, ProviderError> {
    let session_id = required_session_id(session, AgentKind::Claude)?;
    validate_claude_session_id(session_id)?;
    Ok(session_id)
}

fn required_session_id(session: &Session, kind: AgentKind) -> Result<&str, ProviderError> {
    session
        .agent_session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty())
        .ok_or(ProviderError::MissingSessionId {
            agent: agent_name(kind),
        })
}

fn validate_claude_session_id(session_id: &str) -> Result<(), ProviderError> {
    Uuid::parse_str(session_id).map(|_| ()).map_err(|source| {
        ProviderError::InvalidClaudeSessionId {
            id: session_id.to_owned(),
            source,
        }
    })
}

const fn agent_name(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Codex => "Codex",
        AgentKind::Claude => "Claude",
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use serde_json::Value;
    use tempfile::tempdir;

    use super::{
        CommandSpec, LaunchContext, ProviderError, build_new_command, build_resume_command,
        claude_settings_document, codex_hook_overrides, write_claude_settings,
    };
    use crate::config::{AgentConfig, AgentKind};
    use crate::model::{RuntimeState, Session, WorkflowState};

    const CLAUDE_SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const CODEX_SESSION_ID: &str = "018f0000-0000-7000-8000-000000000001";

    #[test]
    fn codex_new_is_interactive_and_preserves_configured_argv() {
        let agent = agent(
            AgentKind::Codex,
            "codex executable;still-one-path",
            &["--model", "value with spaces;$(touch nope)|&'\"", "*.rs"],
        );
        let session = session(None);
        let context = context();

        let command = build_new_command(&agent, &session, &context).unwrap();

        assert_eq!(
            command.program,
            OsStr::new("codex executable;still-one-path")
        );
        assert_eq!(
            &command.args[..3],
            [
                OsString::from("--model"),
                OsString::from("value with spaces;$(touch nope)|&'\""),
                OsString::from("*.rs"),
            ]
        );
        assert_eq!(&command.args[3..], codex_hook_overrides());
        assert!(!command.args.iter().any(|arg| arg == "resume"));
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-hook-trust")
        );
        assert_common_command_fields(&command);
    }

    #[test]
    fn codex_resume_uses_resume_subcommand_and_provider_id() {
        let agent = agent(AgentKind::Codex, "codex", &["--search"]);
        let session = session(Some(CODEX_SESSION_ID));
        let context = context();

        let command = build_resume_command(&agent, &session, &context).unwrap();

        assert_eq!(command.args[0], "--search");
        assert_eq!(
            &command.args[command.args.len() - 2..],
            [OsString::from("resume"), OsString::from(CODEX_SESSION_ID)]
        );
    }

    #[test]
    fn codex_hook_overrides_are_stable_single_argv_values() {
        assert_eq!(
            codex_hook_overrides(),
            [
                OsString::from("-c"),
                OsString::from(
                    r#"hooks.SessionStart=[{hooks=[{type="command",command='"$WIPS_EXECUTABLE" hook'}]}]"#,
                ),
                OsString::from("-c"),
                OsString::from(
                    r#"hooks.UserPromptSubmit=[{hooks=[{type="command",command='"$WIPS_EXECUTABLE" hook'}]}]"#,
                ),
                OsString::from("-c"),
                OsString::from(
                    r#"hooks.Stop=[{hooks=[{type="command",command='"$WIPS_EXECUTABLE" hook'}]}]"#,
                ),
            ]
        );
    }

    #[test]
    fn claude_new_uses_preassigned_uuid_settings_and_preserved_argv() {
        let agent = agent(
            AgentKind::Claude,
            "claude",
            &["--append-system-prompt", "literal `cmd` $(cmd); | & * ?"],
        );
        let session = session(Some(CLAUDE_SESSION_ID));
        let context = context();

        let command = build_new_command(&agent, &session, &context).unwrap();

        assert_eq!(
            command.args,
            [
                OsString::from("--append-system-prompt"),
                OsString::from("literal `cmd` $(cmd); | & * ?"),
                OsString::from("--settings"),
                OsString::from("/state dir/claude-settings.json"),
                OsString::from("--session-id"),
                OsString::from(CLAUDE_SESSION_ID),
            ]
        );
        assert_common_command_fields(&command);
    }

    #[test]
    fn claude_resume_uses_resume_flag() {
        let agent = agent(AgentKind::Claude, "claude", &[]);
        let session = session(Some(CLAUDE_SESSION_ID));
        let context = context();

        let command = build_resume_command(&agent, &session, &context).unwrap();

        assert_eq!(
            command.args,
            [
                OsString::from("--settings"),
                OsString::from("/state dir/claude-settings.json"),
                OsString::from("--resume"),
                OsString::from(CLAUDE_SESSION_ID),
            ]
        );
    }

    #[test]
    fn claude_rejects_missing_or_invalid_session_ids() {
        let agent = agent(AgentKind::Claude, "claude", &[]);
        let context = context();

        let missing = build_new_command(&agent, &session(None), &context).unwrap_err();
        assert!(matches!(missing, ProviderError::MissingSessionId { .. }));

        let invalid =
            build_resume_command(&agent, &session(Some("not-a-uuid")), &context).unwrap_err();
        assert!(matches!(
            invalid,
            ProviderError::InvalidClaudeSessionId { .. }
        ));
    }

    #[test]
    fn claude_settings_use_exec_form_for_all_events() {
        let document = claude_settings_document(Path::new("/opt/wips dir/wips;literal")).unwrap();
        let hooks = document["hooks"].as_object().unwrap();

        assert_eq!(hooks.len(), 3);
        for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
            let handler = &hooks[event][0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], "/opt/wips dir/wips;literal");
            assert_eq!(handler["args"], serde_json::json!(["hook"]));
            assert!(handler.get("shell").is_none());
        }
    }

    #[test]
    fn write_claude_settings_writes_the_native_document() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("claude-settings.json");

        write_claude_settings(&path, Path::new("/usr/local/bin/wips")).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let document: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            document,
            claude_settings_document(Path::new("/usr/local/bin/wips")).unwrap()
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn agent(kind: AgentKind, program: &str, args: &[&str]) -> AgentConfig {
        AgentConfig {
            kind,
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn session(agent_session_id: Option<&str>) -> Session {
        Session {
            id: "wips-session".to_owned(),
            tab_id: "tab".to_owned(),
            position: 0,
            agent: "agent".to_owned(),
            agent_session_id: agent_session_id.map(str::to_owned),
            provider_ready: agent_session_id.is_some(),
            cwd: PathBuf::from("/work tree/literal;$(command)"),
            title: "title".to_owned(),
            transcript_path: None,
            workflow_state: WorkflowState::Open,
            runtime_state: RuntimeState::Creating,
            tmux_pane_id: None,
            exit_code: None,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
        }
    }

    fn context() -> LaunchContext<'static> {
        LaunchContext {
            logical_session_id: "logical-session;$(command)",
            tmux_socket: "wips socket;literal",
            tmux_session: "wips session;literal",
            current_exe: Path::new("/opt/wips dir/wips"),
            state_dir: Path::new("/state dir"),
            config_path: Path::new("/config dir/wips.toml"),
            claude_settings_path: Path::new("/state dir/claude-settings.json"),
        }
    }

    fn assert_common_command_fields(command: &CommandSpec) {
        assert_eq!(command.cwd, Path::new("/work tree/literal;$(command)"));
        assert_eq!(
            command.env[OsStr::new("WIPS_SESSION_ID")],
            "logical-session;$(command)"
        );
        assert_eq!(
            command.env[OsStr::new("WIPS_EXECUTABLE")],
            "/opt/wips dir/wips"
        );
        assert_eq!(command.env[OsStr::new("WIPS_STATE_DIR")], "/state dir");
        assert_eq!(
            command.env[OsStr::new("WIPS_CONFIG")],
            "/config dir/wips.toml"
        );
        assert_eq!(
            command.env[OsStr::new("WIPS_TMUX_SOCKET")],
            "wips socket;literal"
        );
        assert_eq!(
            command.env[OsStr::new("WIPS_TMUX_SESSION")],
            "wips session;literal"
        );
    }
}
