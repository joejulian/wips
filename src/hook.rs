use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::MessageRole;
use crate::paths::Paths;
use crate::store::Store;
use crate::tmux::Tmux;

#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(alias = "sessionId")]
    session_id: String,
    #[serde(default, alias = "transcriptPath")]
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
    #[serde(alias = "hookEventName")]
    hook_event_name: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, alias = "lastAssistantMessage")]
    last_assistant_message: Option<String>,
}

pub(crate) fn handle(paths: &Paths) -> Result<()> {
    let logical_session_id = std::env::var("WIPS_SESSION_ID")
        .context("WIPS_SESSION_ID is missing from the agent hook environment")?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read agent hook input")?;
    let event: HookInput = serde_json::from_str(&input).context("parse agent hook JSON")?;
    handle_event(paths, &logical_session_id, &event)
}

fn handle_event(paths: &Paths, logical_session_id: &str, event: &HookInput) -> Result<()> {
    let store = Store::open(&paths.database)?;
    store.register_hook(
        logical_session_id,
        &event.session_id,
        event.transcript_path.as_deref(),
        &event.cwd,
    )?;

    match event.hook_event_name.as_str() {
        "UserPromptSubmit" => {
            if let Some(prompt) = event
                .prompt
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                store.index_message(logical_session_id, MessageRole::User, prompt)?;
                refresh_title(&store, logical_session_id)?;
            }
        }
        "Stop" => {
            if let Some(message) = event
                .last_assistant_message
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                store.index_message(logical_session_id, MessageRole::Assistant, message)?;
            }
        }
        _ => {}
    }

    Ok(())
}

fn refresh_title(store: &Store, logical_session_id: &str) -> Result<()> {
    let session = store
        .get_session(logical_session_id)?
        .with_context(|| format!("session {logical_session_id} does not exist"))?;
    let Some(pane_id) = session.tmux_pane_id.as_deref() else {
        return Ok(());
    };
    let Some(tmux) = tmux_from_environment()? else {
        return Ok(());
    };
    tmux.set_pane_title(pane_id, &safe_title(&session.title))
}

fn tmux_from_environment() -> Result<Option<Tmux>> {
    let Some(socket) = std::env::var_os("WIPS_TMUX_SOCKET") else {
        return Ok(None);
    };
    let Some(session) = std::env::var_os("WIPS_TMUX_SESSION") else {
        return Ok(None);
    };
    let executable = std::env::current_exe().context("resolve WIPS executable")?;
    Ok(Some(Tmux::new(
        socket.to_string_lossy(),
        session.to_string_lossy(),
        executable,
    )))
}

pub(crate) fn safe_title(value: &str) -> String {
    let mut title = String::new();
    for word in value.split_whitespace() {
        if !title.is_empty() {
            title.push(' ');
        }
        for character in word.chars().filter(|character| !character.is_control()) {
            title.push(character);
            if title.chars().count() >= 80 {
                return title;
            }
        }
    }
    if title.is_empty() {
        "untitled WIP".to_owned()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::{HookInput, safe_title};

    #[test]
    fn title_collapses_whitespace_and_controls() {
        assert_eq!(safe_title("  fix\n\x1b[31m login  "), "fix [31m login");
    }

    #[test]
    fn title_has_a_safe_fallback() {
        assert_eq!(safe_title(" \n\t "), "untitled WIP");
    }

    #[test]
    fn accepts_codex_camel_case_hook_fields() {
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "sessionId": "codex-session",
            "transcriptPath": "/tmp/transcript.jsonl",
            "cwd": "/tmp/project",
            "hookEventName": "Stop",
            "lastAssistantMessage": "done"
        }))
        .expect("Codex hook payload should parse");

        assert_eq!(input.session_id, "codex-session");
        assert_eq!(input.hook_event_name, "Stop");
        assert_eq!(input.last_assistant_message.as_deref(), Some("done"));
    }

    #[test]
    fn accepts_claude_snake_case_hook_fields() {
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "claude-session",
            "transcript_path": "/tmp/transcript.jsonl",
            "cwd": "/tmp/project",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "keep working"
        }))
        .expect("Claude hook payload should parse");

        assert_eq!(input.session_id, "claude-session");
        assert_eq!(input.hook_event_name, "UserPromptSubmit");
        assert_eq!(input.prompt.as_deref(), Some("keep working"));
    }
}
