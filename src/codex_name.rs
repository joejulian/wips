use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tungstenite::{Error as WebSocketError, Message, WebSocket, client};

use crate::paths::Paths;
use crate::store::Store;
use crate::title::normalize_tab_title;
use crate::tmux::Tmux;

const RETRY_DELAY: Duration = Duration::from_secs(1);
const POLL_DELAY: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct CodexNameSync {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CodexNameSync {
    pub(crate) fn start(
        paths: Paths,
        logical_session_id: String,
        codex_program: OsString,
        tmux: Tmux,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("wips-codex-name".to_owned())
            .spawn(move || {
                run(
                    &paths,
                    &logical_session_id,
                    &codex_program,
                    &tmux,
                    &worker_stop,
                );
            })
            .ok();
        Self { stop, worker }
    }

    pub(crate) fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CodexNameSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    paths: &Paths,
    logical_session_id: &str,
    codex_program: &OsStr,
    tmux: &Tmux,
    stop: &AtomicBool,
) {
    let Ok(store) = Store::open(&paths.database) else {
        return;
    };
    let Some(thread_id) = wait_for_thread_id(&store, logical_session_id, stop) else {
        return;
    };

    while !stopped(stop) {
        if let Ok(socket_path) = daemon_socket(codex_program) {
            let _ = observe_names(
                &store,
                logical_session_id,
                &thread_id,
                &socket_path,
                tmux,
                stop,
            );
        }
        wait_while_running(stop, RETRY_DELAY);
    }
}

fn wait_for_thread_id(
    store: &Store,
    logical_session_id: &str,
    stop: &AtomicBool,
) -> Option<String> {
    while !stopped(stop) {
        if let Ok(Some(session)) = store.get_session(logical_session_id) {
            if session.provider_ready {
                return session.agent_session_id.filter(|id| !id.is_empty());
            }
        }
        wait_while_running(stop, POLL_DELAY);
    }
    None
}

#[derive(Deserialize)]
struct DaemonVersion {
    status: String,
    #[serde(rename = "socketPath")]
    socket_path: Option<PathBuf>,
}

fn daemon_socket(codex_program: &OsStr) -> Result<PathBuf> {
    let output = Command::new(codex_program)
        .args(["app-server", "daemon", "version"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("inspect the Codex app-server daemon")?;
    if !output.status.success() {
        bail!("Codex app-server daemon inspection failed");
    }
    let version: DaemonVersion =
        serde_json::from_slice(&output.stdout).context("parse Codex app-server daemon status")?;
    if version.status != "running" {
        bail!("Codex app-server daemon is not running");
    }
    version
        .socket_path
        .context("Codex app-server daemon did not report its socket path")
}

fn observe_names(
    store: &Store,
    logical_session_id: &str,
    thread_id: &str,
    socket_path: &PathBuf,
    tmux: &Tmux,
    stop: &AtomicBool,
) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to Codex app-server at {}", socket_path.display()))?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (mut websocket, _) = client("ws://localhost/", stream)
        .map_err(|error| anyhow::anyhow!("open Codex app-server WebSocket: {error}"))?;

    send_json(
        &mut websocket,
        &json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "wips",
                    "title": "WIPS",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {"experimentalApi": true}
            }
        }),
    )?;
    send_json(
        &mut websocket,
        &json!({"method": "initialized", "params": {}}),
    )?;
    send_json(
        &mut websocket,
        &json!({
            "method": "thread/resume",
            "id": 1,
            "params": {"threadId": thread_id, "excludeTurns": true}
        }),
    )?;
    websocket.get_mut().set_nonblocking(true)?;

    while !stopped(stop) {
        match websocket.read() {
            Ok(message) => match message {
                Message::Text(text) => {
                    if let Ok(message) = serde_json::from_str::<Value>(text.as_ref()) {
                        if resume_failed(&message) {
                            bail!("Codex app-server rejected the thread subscription");
                        }
                        if let Some(title) = name_update(&message, thread_id) {
                            sync_tab_title(store, logical_session_id, title, tmux)?;
                        }
                    }
                }
                Message::Close(_) => return Ok(()),
                _ => {}
            },
            Err(WebSocketError::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                wait_while_running(stop, POLL_DELAY);
            }
            Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => return Ok(()),
            Err(error) => return Err(error).context("read Codex app-server notification"),
        }
    }
    Ok(())
}

fn send_json(websocket: &mut WebSocket<UnixStream>, value: &Value) -> Result<()> {
    let text = serde_json::to_string(value).context("encode Codex app-server request")?;
    websocket
        .send(Message::Text(text.into()))
        .context("send Codex app-server request")
}

fn resume_failed(message: &Value) -> bool {
    message.get("id").and_then(Value::as_i64) == Some(1) && message.get("error").is_some()
}

fn name_update<'a>(message: &'a Value, expected_thread_id: &str) -> Option<&'a str> {
    (message.get("method").and_then(Value::as_str) == Some("thread/name/updated"))
        .then(|| message.get("params"))
        .flatten()
        .filter(|params| params.get("threadId").and_then(Value::as_str) == Some(expected_thread_id))
        .and_then(|params| params.get("threadName"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
}

fn sync_tab_title(
    store: &Store,
    logical_session_id: &str,
    provider_title: &str,
    tmux: &Tmux,
) -> Result<()> {
    let title = normalize_tab_title(provider_title)?;
    let Some(tab_id) = store.rename_single_session_tab(logical_session_id, &title)? else {
        return Ok(());
    };
    let (windows, _) = tmux.snapshots()?;
    if let Some(window) = windows.iter().find(|window| window.tab_id == tab_id) {
        tmux.set_window_title(&window.window_id, &title)?;
    }
    Ok(())
}

fn wait_while_running(stop: &AtomicBool, duration: Duration) {
    let slices = duration.as_millis().div_ceil(POLL_DELAY.as_millis());
    for _ in 0..slices {
        if stopped(stop) {
            break;
        }
        thread::sleep(POLL_DELAY);
    }
}

fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{name_update, resume_failed};

    #[test]
    fn extracts_only_the_expected_thread_name_notification() {
        let update = json!({
            "method": "thread/name/updated",
            "params": {"threadId": "thread-1", "threadName": "Fix the notifier"}
        });
        assert_eq!(name_update(&update, "thread-1"), Some("Fix the notifier"));
        assert_eq!(name_update(&update, "thread-2"), None);
        assert_eq!(
            name_update(
                &json!({
                    "method": "thread/name/updated",
                    "params": {"threadId": "thread-1", "threadName": null}
                }),
                "thread-1"
            ),
            None
        );
    }

    #[test]
    fn detects_a_rejected_subscription() {
        assert!(resume_failed(&json!({
            "id": 1,
            "error": {"code": -1, "message": "not found"}
        })));
        assert!(!resume_failed(&json!({"id": 1, "result": {}})));
    }
}
