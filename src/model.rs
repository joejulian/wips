use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowState {
    Open,
    Completed,
}

impl WorkflowState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeState {
    Creating,
    Running,
    Exited,
    Missing,
}

impl RuntimeState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Tab {
    pub(crate) id: String,
    pub(crate) position: i64,
    pub(crate) title: String,
    pub(crate) layout: Option<String>,
    pub(crate) tmux_window_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) tab_id: String,
    pub(crate) position: i64,
    pub(crate) agent: String,
    pub(crate) agent_session_id: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) title: String,
    pub(crate) transcript_path: Option<PathBuf>,
    pub(crate) provider_ready: bool,
    pub(crate) workflow_state: WorkflowState,
    pub(crate) runtime_state: RuntimeState,
    pub(crate) tmux_pane_id: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) completed_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewSession {
    pub(crate) id: String,
    pub(crate) tab_id: String,
    pub(crate) position: i64,
    pub(crate) agent: String,
    pub(crate) agent_session_id: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchHit {
    pub(crate) session_id: String,
    pub(crate) agent: String,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) role: MessageRole,
    pub(crate) excerpt: String,
    pub(crate) updated_at: i64,
    pub(crate) workflow_state: WorkflowState,
    pub(crate) tmux_pane_id: Option<String>,
    pub(crate) tmux_window_id: Option<String>,
}
